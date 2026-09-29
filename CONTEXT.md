# Bandwidth throttling

This context describes how the project limits network traffic selected by a user.

## Language

**Bandwidth cap**:
The maximum combined transfer rate allowed for selected traffic in one direction. A cap does not reserve bandwidth or guarantee that the traffic reaches the configured rate.
_Avoid_: Bandwidth allocation, guaranteed share

**Selected application**:
The Linux application whose network traffic receives a shared bandwidth cap, including traffic from its child processes. The application does not need to support proxy settings.
_Avoid_: Proxied browser

**Uncapped application**:
An application outside the selected application's cap, using the host's normal network connection. It can use available bandwidth, but the cap does not reserve bandwidth for it.
_Avoid_: Guaranteed-priority application
