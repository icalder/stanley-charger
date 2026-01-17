## Setup on Cerbo GX

```sh
bluetoothctl
scan onoth]# scan on
[bluetooth]# SetDiscoveryFilter success
[bluetooth]# hci1 type 6 discovering on
[bluetooth]# Discovery started
[bluetooth]# [CHG] Controller 10:BB:F3:C5:A0:17 Discovering: yes
[bluetooth]# [NEW] Device E1:4B:E8:B0:75:CA Fence
[bluetooth]# [NEW] Device CF:2A:C7:3C:90:1E House Wall
[bluetooth]# [NEW] Device A5:C2:37:02:49:87 DP04S007L4S100A
[bluetooth]# [NEW] Device E3:66:56:65:1E:15 Carport North
[bluetooth]# [NEW] Device C3:D5:DD:E0:0C:08 Carport South
trust A5:C2:37:02:49:872:37:02:49:87
[bluetooth]# [CHG] Device A5:C2:37:02:49:87 Trusted: yes
[bluetooth]# Changing A5:C2:37:02:49:87 trust succeeded
[bluetooth]# hci1 type 6 discovering off
[bluetooth]# exit
```

## Build

```sh
cargo build --target armv7-unknown-linux-gnueabihf --release
patchelf --set-interpreter /lib/ld-linux-armhf.so.3 ./target/armv7-unknown-linux-gnueabihf/release/stanley-charger
```

## Publish

```sh
scp target/armv7-unknown-linux-gnueabihf/release/stanley-charger root@einstein:
```

## Run

```sh
screen ./stanley-charger
# Ctrl+A D
# Later...
screen -r
screen -ls
```