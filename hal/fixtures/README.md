# hal test fixtures

`gpt-head.bin` / `gpt-tail.bin` — the two partition-table regions of a
16 MiB disk image partitioned by real `sfdisk` (util-linux), laid out like
the boot pendrive (`docs/storage/usb-msc-plan.md`). Read by
`hal/src/gpt.rs`'s tests as an oracle independent of the parser.
Regenerate with:

```bash
truncate -s 16M gpt.img && sfdisk -q gpt.img <<'X'
label: gpt
first-lba: 34
start=34, size=2048, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="boot"
start=4096, size=24576, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="constanos-data"
X
dd if=gpt.img of=gpt-head.bin bs=512 count=34
dd if=gpt.img of=gpt-tail.bin bs=512 skip=$((32768-33)) count=33
```

The GUIDs sfdisk generates differ each run; no test depends on them.

`stick-head.bin` / `stick-tail.bin` — the same two regions read off the
**real** boot pendrive (SanDisk 3.2Gen1, 60088320 sectors) on 2026-09-23:
the first 34 and the last 33 sectors. Regenerate (stick plugged in, found by
label, never by `/dev/sdX` guess):

```bash
DEV=$(lsblk -no PKNAME "$(readlink -f /dev/disk/by-partlabel/constanos-data)")
N=$(sudo blockdev --getsz /dev/$DEV)
sudo dd if=/dev/$DEV of=stick-head.bin bs=512 count=34
sudo dd if=/dev/$DEV of=stick-tail.bin bs=512 skip=$((N-33)) count=33
```
