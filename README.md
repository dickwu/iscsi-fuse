# iscsi-fuse

Mount iSCSI targets as FUSE filesystems on macOS. Uses [iscsi-client-rs](https://github.com/Masorubka1/iscsi-client-rs) for the iSCSI protocol and [macFUSE](https://macfuse.io/) to expose the target as a virtual disk file.

## Prerequisites

### macFUSE

macFUSE is required for iscsi-fuse to work. It provides the kernel extension that enables userspace filesystems on macOS.

**Install macFUSE:**

```bash
brew install --cask macfuse
```

**Enable the system extension:**

After installing macFUSE, you must approve its system extension:

1. Open **System Settings** (System Preferences on older macOS)
2. Go to **Privacy & Security**
3. Scroll to the bottom -- you should see a message: *"System software from 'Benjamin Fleischer' was blocked from loading"*
4. Click **Allow**
5. **Reboot your Mac** (required for the kernel extension to load)

**Verify macFUSE is loaded:**

```bash
kextstat | grep macfuse
# Should show: io.macfuse.filesystems.macfuse (5.x.x)
```

If the kext is not loaded after reboot, try:

```bash
sudo kextload /Library/Filesystems/macfuse.fs/Contents/Extensions/$(sw_vers -productVersion | cut -d. -f1)/macfuse.kext
```

## Installation

### Homebrew (recommended)

```bash
brew tap dickwu/iscsi-fuse
brew install iscsi-fuse
```

### Build from source

Requires Rust 1.85+ and macFUSE (for `fuse.pc`):

```bash
brew install pkg-config
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release
# Binary at: ./target/release/iscsi-fuse
```

## Usage

```bash
# Create a mount point
mkdir -p /tmp/iscsi_mount

# Mount read-only
iscsi-fuse --config config.yaml --mount-point /tmp/iscsi_mount --read-only

# Mount read-write
iscsi-fuse --config config.yaml --mount-point /tmp/iscsi_mount

# The iSCSI target appears as a file:
ls -la /tmp/iscsi_mount/disk.img

# Read data
dd if=/tmp/iscsi_mount/disk.img bs=1m count=10 of=/dev/null

# Unmount
umount /tmp/iscsi_mount
# Or press Ctrl+C in the iscsi-fuse terminal
```

### CLI Options

| Option | Default | Description |
|--------|---------|-------------|
| `-c, --config` | (required) | Path to iSCSI YAML config file |
| `-m, --mount-point` | (required) | FUSE mount point directory |
| `-l, --lun` | `0` | LUN number on the iSCSI target |
| `--read-only` | `false` | Mount in read-only mode |
| `--cache-blocks` | `1024` | LRU cache size in blocks |
| `--device-filename` | `disk.img` | Virtual file name in mount |

## Configuration

Create a `config.yaml` with your iSCSI target details:

```yaml
login:
  identity:
    SessionType: Normal
    InitiatorName: "iqn.2024-01.com.iscsi-fuse:initiator"
    InitiatorAlias: "iscsi-fuse-client"
    TargetName: "iqn.2004-04.com.example:target"
    IsX86: false
  auth:
    AuthMethod: None
  integrity:
    HeaderDigest: None
    DataDigest: None
  flow:
    MaxRecvDataSegmentLength: 262144
    MaxBurstLength: 262144
    FirstBurstLength: 65536
  write_flow:
    InitialR2T: true
    ImmediateData: true
    MaxOutstandingR2T: 1
  ordering:
    DataPDUInOrder: true
    DataSequenceInOrder: true
  recovery:
    ErrorRecoveryLevel: 0
  timers:
    DefaultTime2Wait: 0
    DefaultTime2Retain: 0
  limits:
    MaxConnections: 1
  extensions: {}
  transport:
    TargetAddress: "192.168.1.100:3260"
    TargetPortalGroupTag: 1
runtime:
  MaxSessions: 1
  TimeoutConnection: 30
```

Update `TargetName` and `TargetAddress` to match your iSCSI target.

## Auto-Mount at Login (launchd)

A launchd plist template is included for automatic mounting:

```bash
# Copy the template
cp share/com.github.dickwu.iscsi-fuse.plist ~/Library/LaunchAgents/

# Edit the plist to set your config path, mount point, and username
# (launchd requires absolute paths -- ~ is not expanded)
vim ~/Library/LaunchAgents/com.github.dickwu.iscsi-fuse.plist

# Create the mount point and log directory
mkdir -p ~/iscsi
mkdir -p ~/.local/share/iscsi-fuse

# Load the service (starts immediately)
launchctl load ~/Library/LaunchAgents/com.github.dickwu.iscsi-fuse.plist

# Check status
launchctl list | grep iscsi-fuse

# Stop the service
launchctl unload ~/Library/LaunchAgents/com.github.dickwu.iscsi-fuse.plist
```

The service will:
- Start automatically at login (`RunAtLoad`)
- Restart on crash (but not on clean unmount)
- Wait 30 seconds between restart attempts

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE) for the full text.

This project uses [iscsi-client-rs](https://github.com/Masorubka1/iscsi-client-rs) which is also AGPL-3.0 licensed.
