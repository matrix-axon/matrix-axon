#!/usr/bin/env python3
"""Check the exported artifact, without printing profile contents or secrets."""
import argparse
import pathlib
import plistlib
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
    keychain = info.get("AxonSharedKeychainGroup", "")
    assert keychain.endswith(".org.matrixaxon.axon.sharing"), "Shared Keychain group missing"
    assert keychain in entitlements.get("keychain-access-groups", []), "Signed shared Keychain entitlement missing"
    assert any(keychain == item or (item.endswith("*") and keychain.startswith(item[:-1]))
               for item in allowed.get("keychain-access-groups", [])), "Profile denies shared Keychain group"
    return info, keychain


app, keychain = inspect(args.app)
probes = list((args.app / "PlugIns").glob("*share_probe.appex"))
assert bool(probes) == args.probe, "Development probe embedding differs from requested build"
for probe in probes:
    extension, extension_keychain = inspect(probe)
    assert extension_keychain == keychain, "App/extension Keychain groups differ"
    for key in ["CFBundleVersion", "CFBundleShortVersionString"]:
        assert extension[key] == app[key], "App/extension versions differ"
print("Verified signed App Group, shared Keychain, provisioning, and probe embedding.")
