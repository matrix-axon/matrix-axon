#!/usr/bin/env python3
"""Check the exported artifact, without printing profile contents or secrets."""
import argparse
import json
import pathlib
import plistlib
import re
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("app", type=pathlib.Path)
parser.add_argument("--probe", action="store_true")
args = parser.parse_args()
group = "group.org.matrixaxon.axon.sharing"


def inspect(bundle):
    signed = subprocess.run(
        ["codesign", "--display", "--entitlements", ":-", str(bundle)],
        check=True, capture_output=True,
    )
    entitlements = plistlib.loads(signed.stdout)
    profile = subprocess.run(
        ["security", "cms", "-D", "-i", str(bundle / "embedded.mobileprovision")],
        check=True, capture_output=True,
    )
    allowed = plistlib.loads(profile.stdout)["Entitlements"]
    assert group in entitlements.get("com.apple.security.application-groups", []), "Signed App Group missing"
    assert group in allowed.get("com.apple.security.application-groups", []), "Profile App Group missing"
    info = plistlib.loads((bundle / "Info.plist").read_bytes())
    identifier = entitlements.get("application-identifier", "")
    assert identifier.endswith("." + info["CFBundleIdentifier"]), "Signed bundle identifier differs"
    permitted = allowed.get("application-identifier", "")
    assert identifier == permitted or (permitted.endswith("*") and identifier.startswith(permitted[:-1])), "Profile denies bundle identifier"
    ats = info.get("NSAppTransportSecurity", {})
    assert not any(ats.get(key, False) for key in [
        "NSAllowsArbitraryLoads", "NSAllowsArbitraryLoadsInWebContent",
        "NSAllowsArbitraryLoadsForMedia",
    ]), "Native sharing must retain default ATS protections"
    keychain = info.get("AxonSharedKeychainGroup", "")
    assert re.fullmatch(r"[A-Z0-9]{10}\.org\.matrixaxon\.axon\.sharing", keychain), (
        f"Shared Keychain group in Info.plist is {keychain!r}, expected "
        "'<10-character team id>.org.matrixaxon.axon.sharing'. A value with no team id, or "
        "one still containing '$(', means the build setting it is made from was empty "
        "or unset when Info.plist was processed; "
        "see AxonSharedKeychainGroup in Info.ios.plist and configure-ios-sharing.mjs"
    )
    assert keychain in entitlements.get("keychain-access-groups", []), "Signed shared Keychain entitlement missing"
    assert any(keychain == item or (item.endswith("*") and keychain.startswith(item[:-1]))
               for item in allowed.get("keychain-access-groups", [])), "Profile denies shared Keychain group"
    return info, keychain


app, keychain = inspect(args.app)
probes = list((args.app / "PlugIns").glob("*share_probe.appex"))
assert bool(probes) == args.probe, "Development probe embedding differs from requested build"
shares = list((args.app / "PlugIns").glob("*share.appex"))
assert len(shares) == 1, "Production Share Extension missing or duplicated"
assert set((args.app / "PlugIns").glob("*.appex")) == set(shares + probes), "Unexpected extension bundle"
share_info = plistlib.loads((shares[0] / "Info.plist").read_bytes())
assert share_info["CFBundleIdentifier"] == "org.matrixaxon.axon.Share"
activation = json.loads(pathlib.Path(__file__).with_name("ios-share-activation.json").read_text())
assert share_info["NSExtension"]["NSExtensionAttributes"]["NSExtensionActivationRule"] == activation, "Share activation differs from the OS-tested text/URL rule"
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(args.app)], check=True, capture_output=True)
for probe in probes + shares:
    extension, extension_keychain = inspect(probe)
    assert extension_keychain == keychain, "App/extension Keychain groups differ"
    for key in ["CFBundleVersion", "CFBundleShortVersionString"]:
        assert extension[key] == app[key], "App/extension versions differ"
print("Verified signed App Group, shared Keychain, provisioning, default ATS, production extension signing, activation, and probe embedding.")
