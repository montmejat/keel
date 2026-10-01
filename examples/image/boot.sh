#!/bin/sh
# Boots machines whose only program is keel, in QEMU: `boot.sh 3` starts
# three, their daemons reachable at 127.0.0.1:7411, 7412 and 7413.
# `boot.sh stop` powers them off. Their consoles are in target/image/.
#
# Run from the repository's root, after `cargo build`.
set -e
dir=target/image
mkdir -p $dir

if [ "$1" = stop ]; then
    for pid in $dir/*.pid; do
        [ -e "$pid" ] && kill "$(cat "$pid")" 2>/dev/null
        rm -f "$pid"
    done
    exit 0
fi

./target/debug/keel image $dir/keel.cpio > /dev/null
for n in $(seq 1 "${1:-1}"); do
    port=$((7410 + n))
    rm -f $dir/robot-$n.log
    qemu-system-"$(uname -m)" -enable-kvm -m 1G -smp 2 -display none \
        -kernel /lib/modules/"$(uname -r)"/vmlinuz -initrd $dir/keel.cpio \
        -append "console=ttyS0 quiet keel.ip=10.0.2.15/24 keel.gateway=10.0.2.2 keel.name=robot-$n" \
        -nic user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:$port-:7400 \
        -serial file:$dir/robot-$n.log -pidfile $dir/robot-$n.pid -daemonize
    echo "robot-$n: 127.0.0.1:$port, console in $dir/robot-$n.log"
done
