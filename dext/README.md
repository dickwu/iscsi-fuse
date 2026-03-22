# iscsi-rs DriverKit Extension

This directory will contain the Xcode project for the DriverKit system extension.

## Setup (Phase 3)

1. Open Xcode
2. File → New → Project → DriverKit → Driver
3. Product Name: iscsi-rs-dext
4. Bundle Identifier: com.peilinwu.iscsi-rs
5. Save to this directory
6. Add IOUserBlockStorageDevice framework

## Entitlements Required

- com.apple.developer.driverkit
- com.apple.developer.driverkit.family.block-storage-device
- com.apple.developer.driverkit.transport.userclient

## Architecture

See: docs/superpowers/specs/2026-03-21-iscsi-rs-driverkit-design.md
