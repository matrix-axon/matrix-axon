#import <UIKit/UIKit.h>
#import <objc/runtime.h>
#import "AxonLaunchPayload.h"

// Captures the remote-notification dictionary iOS hands the app when a tap
// launches it. The notification plugin's delegate drops that tap, and this
// runs before that delegate exists: `setDelegate:` is called from
// UIApplicationMain, still inside launch, before either plugin is created.
//
// The dictionary is the wake payload (account id, room id, event id). It is
// not a device token. It is removed when the plugin reads it.

static NSString *const axonLaunchKey = @"axon.remote-push.launch";
static BOOL axonDidFinishSwizzled = NO;
static BOOL (*axonOriginalDidFinish)(id, SEL, UIApplication *, NSDictionary *) = NULL;

static BOOL axonDidFinishLaunching(id self, SEL cmd, UIApplication *application, NSDictionary *options) {
    id remote = options[UIApplicationLaunchOptionsRemoteNotificationKey];
    if ([remote isKindOfClass:[NSDictionary class]]) {
        [[NSUserDefaults standardUserDefaults] setObject:remote forKey:axonLaunchKey];
    }
    if (axonOriginalDidFinish != NULL) {
        return axonOriginalDidFinish(self, cmd, application, options);
    }
    return YES;
}

static void axonSwizzleDidFinish(Class cls) {
    if (axonDidFinishSwizzled || cls == Nil) {
        return;
    }
    SEL sel = @selector(application:didFinishLaunchingWithOptions:);
    Method method = class_getInstanceMethod(cls, sel);
    if (method == NULL) {
        return;
    }
    axonOriginalDidFinish = (BOOL (*)(id, SEL, UIApplication *, NSDictionary *))method_getImplementation(method);
    method_setImplementation(method, (IMP)axonDidFinishLaunching);
    axonDidFinishSwizzled = YES;
}

static void (*axonOriginalSetDelegate)(id, SEL, id) = NULL;

static void axonSetDelegate(id self, SEL cmd, id delegate) {
    if (delegate != nil) {
        axonSwizzleDidFinish(object_getClass(delegate));
    }
    if (axonOriginalSetDelegate != NULL) {
        axonOriginalSetDelegate(self, cmd, delegate);
    }
}

__attribute__((constructor)) static void axonRemotePushBoot(void) {
    Class cls = [UIApplication class];
    SEL sel = @selector(setDelegate:);
    Method method = class_getInstanceMethod(cls, sel);
    if (method == NULL) {
        return;
    }
    axonOriginalSetDelegate = (void (*)(id, SEL, id))method_getImplementation(method);
    method_setImplementation(method, (IMP)axonSetDelegate);
}

void axon_remote_push_link(void) {}
