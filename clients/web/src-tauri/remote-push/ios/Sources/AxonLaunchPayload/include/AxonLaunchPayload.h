#ifndef AXON_LAUNCH_PAYLOAD_H
#define AXON_LAUNCH_PAYLOAD_H

/// Does nothing. The launch-payload capture lives in a constructor in
/// LaunchPayload.m, and a static library's object file is only linked when
/// something references it. Calling this from Swift is that reference.
void axon_remote_push_link(void);

#endif
