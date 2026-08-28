# Local compatibility patch

This directory vendors `apple-cf` 0.9.3 from
<https://github.com/doom-fish/apple-cf-rs> under its original MIT OR
Apache-2.0 license.

The only LCXL source change is in
`swift-bridge/Sources/CoreVideoBridge/CoreVideo.swift`: Xcode 26 imports
`IOSurface` as a Swift wrapper while `CVPixelBufferCreateWithIOSurface` still
accepts the CF-style `IOSurfaceRef` alias. The bridge now performs the explicit
identity cast required by that SDK. Remove the Cargo patch and this directory
after an upstream release contains an equivalent fix.
