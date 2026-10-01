# 0023: keel as process 1: a machine that is a kernel and keel

**Status:** Accepted, 2026-10-01 (proposed 2026-10-01)

## Context
Provisioning ([0018](0018-provisioning.md)) installs keel on a machine that
already runs a distribution: systemd starts the daemon, SSH gets it there.
[0012](0012-daemon-as-init.md) calls the daemon a small init for its nodes.
Taken literally: what's the least a machine needs under keel? A kernel.
This is the bottom of the stack, and the kind of thing that never ends, so
it's cut to what makes one dataflow run.

## Decision
- **`keel image` builds an initramfs**: one file a kernel unpacks into
  memory and whose `/init` it runs. It holds `keel` (already static,
  [0016](0016-packaging-and-deployment.md)) as `/init`, the kernel modules
  asked for with their dependencies (`modprobe --show-depends`, decompressed
  here), and the token. 4.5 MB for QEMU's network card.
- **The archive is written by hand** (cpio's "new ASCII" format, about 30
  lines): it needs `/dev/console` in it, a device node, which takes root to
  create on disk but not to describe in an archive. Times are zero, so the
  image is reproducible and has a hash like everything else.
- **When keel is process 1 it's the init**: it mounts `/proc`, `/sys`,
  `/dev` and memory file systems for shared memory and sockets, loads the
  modules, sets the address and default route from the kernel command line
  (`keel.ip=`, `keel.gateway=`, `keel.name=`, `keel.listen=`), raises the
  memory-lock limit, and runs `keel daemon`.
- **The daemon is a child, not process 1 itself.** Process 1 has to reap
  every orphan on the machine, and waiting for "any child" would take the
  daemon's own nodes' exits from under it. So process 1 only waits, and
  starts the daemon again if it dies.
- **Nothing is fatal** in process 1 (the kernel panics if it exits): what
  fails is said on the console and the rest goes on.

## Consequences
- Tried in QEMU with the laptop's own kernel: the daemon answers a second
  after power-on, and the camera pipeline deploys to it and runs, with a
  real-time node.
- Everything is in memory: the store is empty at every boot, and deploying
  sends the binaries again. A disk to mount is the next step.
- The address is static. The kernel can do DHCP itself (`ip=dhcp`) when
  built for it; distribution kernels usually aren't.
- The image is tied to a kernel: its modules load only into the release
  they were built for. A kernel with the drivers built in needs none.
- No way in but keel: no shell, no SSH. `keel logs` and the console are
  the only views. There's no clean power-off either: nothing to lose yet,
  with no disk.
- Not tried: the daemon dying and being started again, orphans being
  reaped, a real board (the Pi needs its own kernel and device tree on the
  SD card's boot partition).
- Not done, and where it would never end: a disk and updates of the image
  itself (A/B), DHCP, time, more drivers, a kernel of our own.
