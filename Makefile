OBJCOPY ?= aarch64-linux-gnu-objcopy
TARGET  := aarch64-unknown-none-softfloat
USER_ELF := user/target/$(TARGET)/release/metal-user
KERNEL   := kernel/target/$(TARGET)/release/metal-kernel
# QEMU loads the kernel as an ELF file (entry point and load address come from the ELF headers).
KERNEL_BIN := $(KERNEL)
DISK     ?= disk.img
# the interactive desktop runs with more memory: every fullscreen program has a 3 MiB surface
UI_RAM_MIB ?= 128
SMP ?= 4
RAM_MIB ?= 64
# System-image compression (see tools/mkdisk.py); the 1-CPU test run uses LZ4 so both decoders run in QEMU.
EROFS_OPTS ?= -zzstd,level=19 -C16384
export EROFS_OPTS

# With performance cores online (3+ CPUs) the Heavy task must have stayed on them.
ifeq ($(shell test $(SMP) -ge 3 && echo y),y)
SCHED_PAT := 'TEST Heavy work stays on performance cores when they are not oversubscribed.*PASS'
else
SCHED_PAT := '(Heavy): no performance cores online'
endif
# Host port forwarded to the guest's TCP port 7777 (the inbound connection of the network tests); tools/netpeer.py uses it.
NET_FWD_PORT ?= 18077
QEMU_DEV := -drive if=none,id=d0,file=$(DISK),format=raw -device virtio-blk-pci,drive=d0,disable-legacy=on,iommu_platform=on \
            -device virtio-gpu-pci,disable-legacy=on,iommu_platform=on,xres=1024,yres=768 \
            -device virtio-keyboard-pci,disable-legacy=on,iommu_platform=on \
            -device virtio-tablet-pci,disable-legacy=on,iommu_platform=on \
            -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$(NET_FWD_PORT)-:7777 -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
            -device virtio-rng-pci,disable-legacy=on,iommu_platform=on
# (a fresh 32 random bytes for every make run: the kernel mixes them into its layout generator, see kernel/src/rand.rs)
SEED_FILE := $(shell head -c 32 /dev/urandom > build-seed.bin && echo build-seed.bin)
QEMU     := qemu-system-aarch64 -fw_cfg name=opt/metal/seed,file=$(SEED_FILE) -machine virt,gic-version=2,iommu=smmuv3 -cpu max -smp $(SMP) -m $(RAM_MIB)M -fw_cfg name=opt/metal/ram-bytes,string=$(shell echo $$(( $(RAM_MIB) * 1048576 ))) \
            -nographic -semihosting $(QEMU_DEV) -kernel $(KERNEL_BIN)

# The command line the tests use (for the scripts in tools/ that start the guest themselves)
print-qemu:
	@echo '$(QEMU)'

# The network tests need the host peer (tools/netpeer.py) running while the guest boots.
WITH_PEER := tools/with-peer.sh $(NET_FWD_PORT)

# Chaos test: a monkey kills services at random while a workload uses them; see user/src/chaos.rs.
CHAOS_SEED ?= 1
chaos: kernel disk
	@$(WITH_PEER) sh -c 'timeout 200 $(QEMU) -fw_cfg name=opt/metal/chaos,string=$(CHAOS_SEED) > build-chaos.log 2>&1'; rc=$$?; \
	 grep -E "^\[(chaos|monkey)\].*(TEST|done|CORRUPT|LOST|workload|free frames)" build-chaos.log; \
	 [ $$(grep -c "^\[chaos\] TEST.*PASS" build-chaos.log) -ge 7 ] && \
	 ! grep -qE "^\[chaos\].*(FAIL|CORRUPT|LOST)" build-chaos.log && \
	 grep -q "^\[chaos\] done" build-chaos.log && \
	 echo "== CHAOS (seed $(CHAOS_SEED)): ALL PASS ==" || { echo "== CHAOS (seed $(CHAOS_SEED)): FAILED (qemu rc=$$rc), see build-chaos.log =="; exit 1; }


.PHONY: stack-guard aslr-replay print-qemu quick-programs programs fuzz quick-bench quick-sched qemu-test-noncoherent chaos all user kernel disk erofs-check map-check qemu-test-up inherit-ab provenance-check run run-window test host-test qemu-test clean

all: kernel

user:
	cd user && cargo build --release
	mkdir -p user/build
	cp $(USER_ELF) user/build/user.elf

kernel: user
	cd kernel && cargo build --release

# Programs started with SPAWN (programs/): position-independent ELF files in programs/build (random image base each run; the
# loader relocates them), packed into the system image under /bin. `sleeper` stays a flat image (the user crate embeds
# it); the static `.elf` copies of argdump and hello exist to test the loader's fixed-address path.
PROGRAM_BINS := guardtest pager pagetest hello crash ping mem burn spawner cat fswrite ls fdprobe kv kvc blocker argdump upper threads stdtest nettest netcheck fetch quotatest shmtest pietest progmgr progctl counter crasher hog probe fsprog
PIE_FLAGS := -C relocation-model=pic -C link-arg=-pie -C link-arg=--no-dynamic-linker -C link-arg=-znorelro --cfg getrandom_backend=\"custom\"
programs:
	rm -rf programs/build; mkdir -p programs/build
	cd programs && cargo build --release --bin sleeper
	$(OBJCOPY) -O binary programs/target/$(TARGET)/release/sleeper programs/build/sleeper.bin
	cd programs && cargo build --release
	for a in argdump hello; do cp programs/target/$(TARGET)/release/$$a programs/build/$$a.elf; done
	cd programs && METAL_PIE=1 RUSTFLAGS="$(PIE_FLAGS)" cargo build --release --target-dir target-pie
	for a in $(PROGRAM_BINS); do cp programs/target-pie/$(TARGET)/release/$$a programs/build/$$a.pie; done

disk: programs
	python3 tools/mkdisk.py $(DISK)

run: kernel disk
	$(QEMU)

host-test:
	cargo test --workspace

# Independent check: the Rust EROFS reader must parse the Python-built image.
erofs-check:
	python3 tools/mkdisk.py $(DISK) build-sys.erofs
	cargo run -q --release -p metal-erofs --example erofscat -- build-sys.erofs /hello.txt
	@rm -f build-sys.erofs

# Independent check: the Rust package parser must accept what tools/mkmap.py builds (and refuse the flawed fixtures).
map-check: programs
	python3 tools/mkmap.py
	@for f in counter crasher hog probefs probenofs fsa fsb nosign mismatch rogue tampered; do cargo run -q --release -p metal-map --example mapinfo -- programs/build/maps/$$f.map | grep -q '^ok ' || { echo "map-check: $$f.map not accepted"; exit 1; }; done
	@cargo run -q --release -p metal-map --example mapinfo -- programs/build/maps/corrupt.map | grep -q 'refused BadChecksum' || { echo "map-check: corrupt.map not refused"; exit 1; }
	@echo "== MAP CHECK: ALL PASS =="

# Boot in QEMU and assert on the console transcript.
qemu-test: kernel disk
	@$(WITH_PEER) sh -c 'timeout 260 $(QEMU) > build.log 2>&1'; rc=$$?; cat build.log; \
	 grep -q "TEST zero-copy shared memory.*PASS" build.log && \
	 grep -q "TEST async IPC_SEND.*PASS" build.log && \
	 grep -q "TEST capability revoke.*PASS" build.log && \
	 grep -q "TEST capability grants are bounded.*PASS" build.log && \
	 grep -q "TEST the stack is fully usable.*PASS" build.log && \
	 grep -q "TEST every program gets its own randomised layout.*PASS" build.log && \
	 grep -q "TEST a thread on a kernel-made stack.*PASS" build.log && \
	 grep -q "TEST MEM_MAP with address 0 lets the kernel choose.*PASS" build.log && \
	 grep -q "TEST the pages next to a kernel-placed mapping fault.*PASS" build.log && \
	 grep -q "TEST pages go from the compressed store to the disk.*PASS" build.log && \
	 grep -q "TEST a restarted pager takes over.*PASS" build.log && \
	 grep -q "TEST every page comes back from disk.*PASS" build.log && \
	 grep -q "TEST a program that keeps using its pages while they are pushed to disk.*PASS" build.log && \
	 grep -q "TEST nothing is left behind when the programs are gone.*PASS" build.log && \
	 grep -q "TEST driver crash surfaces as error.*PASS" build.log && \
	 grep -q "TEST driver restarted with fresh state.*PASS" build.log && \
	 grep -q "TEST sleep/timer.*PASS" build.log && \
	 grep -q "TEST virtio-blk read sector 0.*PASS" build.log && \
	 grep -q "TEST virtio-blk write+readback.*PASS" build.log && \
	 grep -q "TEST IOMMU stops a driver.*PASS" build.log && \
	 grep -q "TEST a block device that never answers.*PASS" build.log && \
	 grep -q "TEST the block driver serves requests again after the watchdog.*PASS" build.log && \
	 grep -q "TEST a display device that never answers.*PASS" build.log && \
	 grep -q "TEST the display driver presents again after the watchdog.*PASS" build.log && \
	 grep -q "TEST an interrupted rewrite leaves the previous version.*PASS" build.log && \
	 grep -q "TEST a completed rewrite replaces the file.*PASS" build.log && \
	 grep -q "TEST a sparse write reads back zeros.*PASS" build.log && \
	 grep -q "TEST HEAP_GROW with a wrapping page count is refused.*PASS" build.log && \
	 grep -q "TEST an ordinary client cannot use the VFS fault-injection hook.*PASS" build.log && \
	 grep -q "TEST file size is bounded.*PASS" build.log && \
	 grep -q "TEST display: a test card is drawn.*PASS" build.log && \
	 grep -q "TEST system information screen is drawn.*PASS" build.log && \
	 grep -q "TEST IOMMU stops the display driver.*PASS" build.log && \
	 grep -q "TEST restarted display driver presents.*PASS" build.log && \
	 grep -q "TEST the marker page never reached the disk.*PASS" build.log && \
	 grep -q "TEST restarted block driver serves requests again.*PASS" build.log && \
	 grep -q "\[smmu\] blocked DMA: F_TRANSLATION" build.log && \
	 grep -q "TEST blk driver crash surfaces as error.*PASS" build.log && \
	 grep -q "TEST data survives blk driver restart.*PASS" build.log && \
	 grep -q "TEST EROFS read /hello.txt.*PASS" build.log && \
	 grep -q "TEST EROFS read 9000-byte file across blocks: PASS" build.log && \
	 grep -q "TEST EROFS list /.*PASS" build.log && \
	 grep -q "TEST EROFS missing path rejected.*PASS" build.log && \
	 grep -q "TEST LFS write coalescing.*PASS" build.log && \
	 grep -q "TEST LFS server crash surfaces as error.*PASS" build.log && \
	 grep -q "TEST LFS synced blocks survive restart, unsynced block lost: PASS" build.log && \
	 grep -q "TEST LFS sync across a block-driver crash: PASS" build.log && \
	 grep -q "TEST VFS read /system/hello.txt: PASS" build.log && \
	 grep -q "TEST VFS read 9000-byte file via fd: PASS" build.log && \
	 grep -q "TEST VFS read 220 KB file from the compressed system image.*PASS" build.log && \
	 grep -q "TEST VFS list / and /system.*PASS" build.log && \
	 grep -q "TEST VFS /system is read-only.*PASS" build.log && \
	 grep -q "TEST VFS create+write+read /data/docs/note.txt.*PASS" build.log && \
	 grep -q "TEST VFS overwrite across a block boundary: PASS" build.log && \
	 grep -q "TEST VFS list, rmdir-nonempty refused.*PASS" build.log && \
	 grep -q "TEST VFS data survives vfs-server crash; old fd rejected.*PASS" build.log && \
	 grep -q "TEST VFS data survives lfs-server + blk-driver crashes.*PASS" build.log && \
	 grep -q "TEST VFS sustained writes.*PASS" build.log && \
	 grep -q "TEST VFS program task's files are visible.*PASS" build.log && \
	 grep -q "\[program\] TEST read /system/etc/os-release.*PASS" build.log && \
	 grep -q "\[program\] TEST write + append + read back.*PASS" build.log && \
	 grep -q "\[program\] TEST list /data.*PASS" build.log && \
	 grep -q "\[program\] TEST /system rejects writes.*PASS" build.log && \
	 grep -q "\[program\] TEST no direct device or filesystem access.*PASS" build.log && \
	 grep -q "TEST async ring setup.*PASS" build.log && \
	 grep -q "TEST ring batch: 3 NOPs, 1 syscall.*PASS" build.log && \
	 grep -q "TEST ring: 4 concurrent async calls + timeout, 1 syscall.*PASS" build.log && \
	 grep -q "TEST ring: completions reaped by polling.*PASS" build.log && \
	 grep -q "TEST priority inheritance.*PASS" build.log && \
	 grep -q "TEST ring: async call to a crashing driver.*PASS" build.log && \
	 grep -q "TEST ring backpressure.*PASS" build.log && \
	 grep -q "TEST memory pressure.*PASS" build.log && \
	 grep -q "TEST fragmentation.*PASS" build.log && \
	 grep -q "TEST contiguous 512 KiB allocation fails on fragmented memory.*PASS" build.log && \
	 grep -q "TEST compactd restores contiguity.*PASS" build.log && \
	 grep -q "TEST every page read during migration held its data.*PASS" build.log && \
	 grep -q "TEST pages relocated by the compactor keep their contents.*PASS" build.log && \
	 grep -q "TEST contiguous 512 KiB block allocated after compaction.*PASS" build.log && \
	 grep -q "TEST direct compaction inside the allocator.*PASS" build.log && \
	 grep -q $(SCHED_PAT) build.log && \
	 { [ $(SMP) -lt 3 ] || grep -q "TEST a CPU hog that was pushed onto an efficiency core goes back to a performance core once one is idle.*PASS" build.log; } && \
	 grep -q "TEST scheduler shares the cores fairly.*PASS" build.log && \
	 grep -q "TEST scheduler keeps the cores busy.*PASS" build.log && \
	 grep -q "TEST real-time task meets its deadlines.*PASS" build.log && \
	 grep -q "TEST real-time task is dispatched within 1 ms.*PASS" build.log && \
	 grep -q "TEST an overrunning real-time task gets its reserved.*PASS" build.log && \
	 grep -q "TEST real-time admission refuses.*PASS" build.log && \
	 grep -q "TEST a reservation needs the scheduling capability.*PASS" build.log && \
	 grep -q "TEST waiting for the next period without a reservation is refused.*PASS" build.log && \
	 { grep -q "TEST sleeping cores take no periodic tick.*PASS" build.log || [ $(SMP) -lt 3 ]; } && \
	 { grep -q "TEST a real-time caller's urgency reaches the server it calls.*PASS" build.log || [ $(SMP) -gt 1 ]; } && \
	 ! grep -q "TEST \(scheduler\|real-time\|a light task\|hogs take turns\|sustained\|sleeping cores\|a CPU hog that was pushed\).*FAIL" build.log && \
	 grep -q "TEST a program runs and its exit code comes back through WAIT.*PASS" build.log && \
	 grep -q "TEST a program that faults ends with the fault code.*PASS" build.log && \
	 grep -q "TEST a running program: WAIT without blocking.*PASS" build.log && \
	 grep -q "TEST a program uses the endpoint its parent delegated.*PASS" build.log && \
	 grep -q "TEST a program gets exactly the heap it was given.*PASS" build.log && \
	 grep -q "TEST a program with the spawn capability starts programs, and they end with it.*PASS" build.log && \
	 grep -q "TEST a program started with file access reads a file through the VFS.*PASS" build.log && \
	 grep -q "TEST a program writes a file that the conductor then reads.*PASS" build.log && \
	 grep -q "TEST a program's file descriptors do not outlive it.*PASS" build.log && \
	 grep -q "TEST eight file sessions exist.*PASS" build.log && \
	 grep -q "TEST two clients use a server built on an endpoint.*PASS" build.log && \
	 grep -q "TEST revoking the server's capability wakes it.*PASS" build.log && \
	 grep -q "TEST a caller waiting on an endpoint that ends is told so.*PASS" build.log && \
	 grep -q "TEST a task may own four endpoints at a time.*PASS" build.log && \
	 grep -q "TEST program packages in the system image install.*PASS" build.log && \
	 grep -q "TEST a corrupt package, one whose id differs.*PASS" build.log && \
	 grep -q "TEST signatures: an unsigned package, one signed by a key.*PASS" build.log && \
	 grep -q "TEST the manifest's limits and permissions, and that it was signed.*PASS" build.log && \
	 grep -q "TEST a program starts and its UI ability comes to the foreground.*PASS" build.log && \
	 grep -q "TEST on_create and on_foreground ran.*PASS" build.log && \
	 grep -q "TEST starting it again is refused.*PASS" build.log && \
	 grep -q "TEST the manager moves it to the background.*PASS" build.log && \
	 grep -q "TEST \.\.\. and back to the foreground.*PASS" build.log && \
	 grep -q "TEST the ability's own work (on_tick) runs.*PASS" build.log && \
	 grep -q "TEST a crashing program is restarted by the manager.*PASS" build.log && \
	 grep -q "TEST the restarted instance got what the crashed one had saved.*PASS" build.log && \
	 grep -q "TEST the other program did not notice.*PASS" build.log && \
	 grep -q "TEST a crash does not stop the crasher for good.*PASS" build.log && \
	 grep -q "TEST a program that runs into its memory limit dies alone.*PASS" build.log && \
	 grep -q "TEST \.\.\. while the others keep running.*PASS" build.log && \
	 grep -q "TEST a program sees files only with the files permission.*PASS" build.log && \
	 grep -q "TEST stopping asks the ability to leave.*PASS" build.log && \
	 grep -q "TEST stopping what is not running is refused.*PASS" build.log && \
	 grep -q "TEST a program's storage is private.*PASS" build.log && \
	 grep -q "TEST each program's files are where the manager put them.*PASS" build.log && \
	 grep -q "TEST \.\.\. and a program that makes files until it is stopped.*PASS" build.log && \
	 grep -q "TEST uninstalling a program removes its data.*PASS" build.log && \
	 grep -q "TEST a program that is not running can be uninstalled.*PASS" build.log && \
	 grep -q "TEST a program can be installed again after being uninstalled.*PASS" build.log && \
	 grep -q "TEST the program manager scenario ran to its end.*PASS" build.log && \
	 grep -q "TEST nothing is left behind once the program manager is gone.*PASS" build.log && \
	 grep -q "TEST a program receives its command line (name first), environment.*PASS" build.log && \
	 grep -q "TEST a program reads its standard input line by line.*PASS" build.log && \
	 grep -q "TEST 5000 bytes of input and output cross the console endpoint intact.*PASS" build.log && \
	 grep -q "TEST an ELF executable is loaded and runs like a flat image.*PASS" build.log && \
	 grep -q "TEST malformed ELF files are refused.*PASS" build.log && \
	 grep -q "TEST a position-independent ELF executable runs like a flat image.*PASS" build.log && \
	 grep -q "TEST position-independent programs get a random image base.*PASS" build.log && \
	 grep -q "TEST all free task slots can be used.*PASS" build.log && \
	 grep -q "TEST malformed images are refused by the loader.*PASS" build.log && \
	 grep -q "TEST the kernel refuses hostile TASK_LOAD.*PASS" build.log && \
	 grep -q "TEST the std-like layer.*PASS" build.log && \
	 grep -q "TEST standard output and standard error arrive separately.*PASS" build.log && \
	 grep -q "TEST the std test left nothing behind.*PASS" build.log && \
	 grep -q "TEST net: the machine got an address by DHCP.*PASS" build.log && \
	 grep -q "TEST net: names: dotted quads.*PASS" build.log && \
	 grep -q "TEST net: UDP datagrams of 0 to 1472.*PASS" build.log && \
	 grep -q "TEST net: 1 MiB goes out and comes back.*PASS" build.log && \
	 grep -q "TEST net: 2 MiB streamed by the host.*PASS" build.log && \
	 grep -q "TEST net: 2 MiB sent over TCP, half-closed.*PASS" build.log && \
	 grep -q "TEST net: connecting to a port nobody listens on.*PASS" build.log && \
	 grep -q "TEST net: a listener accepts a connection.*PASS" build.log && \
	 grep -q "TEST net: 60 connections in a row.*PASS" build.log && \
	 grep -q "TEST memory pressure makes compactd swap cold pages out.*PASS" build.log && \
	 grep -q "TEST net: four threads with a connection.*PASS" build.log && \
	 grep -q "TEST net: waiting is sleeping.*PASS" build.log && \
	 grep -q "TEST net: a peer that answers in small slow pieces.*PASS" build.log && \
	 grep -q "TEST net: a program that leaves sockets open.*PASS" build.log && \
	 grep -q "TEST net: \.\.\. and the next program can use the same ports.*PASS" build.log && \
	 grep -q "TEST net: when the network service dies.*PASS" build.log && \
	 grep -q "TEST net: nothing is left behind (frames.*PASS" build.log && \
	 grep -q "TEST five tasks of one program count under a lock.*PASS" build.log && \
	 grep -q "TEST two tasks alternate through one waited-on word.*PASS" build.log && \
	 grep -q "TEST a word wait reports a changed value.*PASS" build.log && \
	 grep -q "TEST a fault in one thread ends the whole program.*PASS" build.log && \
	 grep -q "TEST a program whose main task exits ends its sleeping threads.*PASS" build.log && \
	 grep -q "TEST a thread calling exit ends the program.*PASS" build.log && \
	 grep -q "TEST the thread allowance holds.*PASS" build.log && \
	 grep -q "TEST a program started without a thread allowance.*PASS" build.log && \
	 grep -q "TEST an allowance above the limit is refused.*PASS" build.log && \
	 grep -q "TEST a thread reads a file through its program.*PASS" build.log && \
	 grep -q "TEST 150 threads one after another.*PASS" build.log && \
	 grep -q "TEST threads grow their program's heap.*PASS" build.log && \
	 grep -q "TEST killing a program stops its spinning threads.*PASS" build.log && \
	 grep -q "TEST every frame and capability node the threads used is back.*PASS" build.log && \
	 grep -q "TEST 300 programs later every frame, capability node and task slot is back.*PASS" build.log && \
	 grep -q "TEST a program that allocates until refused stays within its limit.*PASS" build.log && \
	 grep -q "TEST a program that allocates without checking is stopped at its limit.*PASS" build.log && \
	 grep -q "TEST contiguous allocations count against the program's limit.*PASS" build.log && \
	 grep -q "TEST a program with a budget of 700 pages starts two default programs.*PASS" build.log && \
	 grep -q "TEST a program may have at most 8 programs at a time.*PASS" build.log && \
	 grep -q "TEST a program costs its heap + budget + 24 pages.*PASS" build.log && \
	 grep -q "TEST a child's limits can be raised within the parent's budget.*PASS" build.log && \
	 grep -q "TEST every frame and capability node the memory-limit tests used is back.*PASS" build.log && \
	 grep -q "TEST SHM_CREATE: sizes, the charge against the limit.*PASS" build.log && \
	 grep -q "TEST shared memory outlives its creator while another program holds it.*PASS" build.log && \
	 grep -q "TEST a program shares memory it created with another.*PASS" build.log && \
	 grep -q "TEST a capability granted without the write right.*PASS" build.log && \
	 grep -q "TEST a holder of shared memory exiting does not free it.*PASS" build.log && \
	 grep -q "TEST when the creator takes shared memory back.*PASS" build.log && \
	 grep -q "TEST a program sleeping on a word of shared memory is woken.*PASS" build.log && \
	 grep -q "TEST shared memory dies with its creator.*PASS" build.log && \
	 grep -q "TEST a 4096-page object is usable.*PASS" build.log && \
	 grep -q "TEST nothing is left behind by the shared-memory tests.*PASS" build.log && \
	 grep -q "TEST shared memory needs no contiguous run.*PASS" build.log && \
	 ! grep -aq "TEST.*program.*FAIL" build.log && \
	 python3 tools/checkdisk.py $(DISK) && \
	 grep -q "zram\] lz4: .*swap-in verified: PASS" build.log && \
	 grep -q "zram\] zstd: .*swap-in verified: PASS" build.log && \
	 grep -q "worker\] heartbeat" build.log && \
	 echo "== QEMU INTEGRATION: ALL PASS ==" || { echo "== QEMU INTEGRATION: FAILED (qemu rc=$$rc) =="; exit 1; }

# Development shortcuts: boot, run one group of the conductor's tests and stop (see platform.rs `only`).
quick-bench: kernel disk
	@timeout 60 $(QEMU) -fw_cfg name=opt/metal/only,string=1 2>&1 | grep -E "BENCH|PANIC|panic|^\[sched\] [0-9]"

# Syscall fuzzer (user/src/fuzz.rs): hostile calls from a task with a mixed set of capabilities, then a leak audit.
fuzz: kernel disk
	@timeout 120 $(QEMU) -fw_cfg name=opt/metal/fuzz,string=$(or $(SEED),1) > build-fuzz.log 2>&1; rc=$$?; \
	 grep -aE "fuzz|\[client\] TEST|PANIC|panic" build-fuzz.log; \
	 [ $$(grep -ac "^\[client\] TEST.*PASS" build-fuzz.log) -ge 5 ] && \
	 ! grep -aqE "TEST.*FAIL|PANIC|panic" build-fuzz.log && \
	 echo "== FUZZ (seed $(or $(SEED),1)): ALL PASS ==" || { echo "== FUZZ (seed $(or $(SEED),1)): FAILED (qemu rc=$$rc), see build-fuzz.log =="; exit 1; }

quick-programs: kernel disk
	@timeout 90 $(QEMU) -fw_cfg name=opt/metal/only,string=7 2>&1 | grep -aE "TEST|\[client\]|PANIC|panic|\[hello|\[crash"

quick-net: kernel disk
	@$(WITH_PEER) sh -c 'timeout 200 $(QEMU) -fw_cfg name=opt/metal/only,string=8 2>&1 | grep -aE "TEST|\[client\]|\[nettest|\[netd|PANIC|panic"'

quick-sched: kernel disk
	@timeout 90 $(QEMU) -fw_cfg name=opt/metal/only,string=2 2>&1 | grep -E "SCHED|TEST|^\[sched\]|PANIC|panic|DEBUG"

# uniprocessor configuration too: no PSCI cores, no performance cores
qemu-test-up:
	@$(MAKE) -s qemu-test SMP=1 EROFS_OPTS='-zlz4hc'

# The same boot with the SMMU declared non-coherent: tables and queues are then programmed non-cacheable and every
# CPU write the SMMU must see is cleaned first (what a device that does not snoop the CPU caches needs). QEMU's
# models ignore the attributes, so this checks that the maintenance path runs and breaks nothing.
qemu-test-noncoherent: kernel disk
	@$(WITH_PEER) sh -c 'timeout 260 $(QEMU) -fw_cfg name=opt/metal/dma-noncoherent,string=1 > build-noncoherent.log 2>&1'; rc=$$?; \
	 grep -E "SMMUv3 enabled|TEST virtio-blk write|TEST IOMMU stops|TEST the display driver|FAIL" build-noncoherent.log | cut -c1-200; \
	 grep -q "not coherent" build-noncoherent.log && ! grep -q "FAIL" build-noncoherent.log && \
	 grep -q "TEST virtio-blk write+readback.*PASS" build-noncoherent.log && \
	 grep -q "TEST IOMMU stops a driver.*PASS" build-noncoherent.log && \
	 grep -q "TEST the display driver presents again after the watchdog.*PASS" build-noncoherent.log && \
	 echo "== NON-COHERENT DMA: ALL PASS ==" || { echo "== NON-COHERENT DMA: FAILED (qemu rc=$$rc), see build-noncoherent.log =="; exit 1; }

# Layout randomisation is reproducible: the same seed gives the boot tasks the same stacks and heaps, another seed does not,
# and the seeds from the harness (no seed given) give different layouts every boot.
aslr-replay: kernel disk
	@run() { timeout 120 $(QEMU) -fw_cfg name=opt/metal/only,string=2 $$1 2>&1 | grep -a "layout of task" ; } ; \
	 a=$$(run "-fw_cfg name=opt/metal/aslr-seed,string=12345"); b=$$(run "-fw_cfg name=opt/metal/aslr-seed,string=12345"); c=$$(run "-fw_cfg name=opt/metal/aslr-seed,string=12346"); \
	 n=$$(echo "$$a" | grep -c .); \
	 [ "$$n" -ge 10 ] && [ "$$a" = "$$b" ] && [ "$$a" != "$$c" ] && echo "== ASLR REPLAY: ALL PASS ($$n boot tasks, same seed same layouts, other seed other layouts) ==" || { echo "== ASLR REPLAY: FAILED ($$n lines) =="; exit 1; }

# Entropy: a normal boot folds in the virtio-rng device's bytes and warns about nothing; the modes that give up
# unpredictability (noaslr, aslr-seed, aslr-log) say so in the boot log and leave the device alone.
entropy: kernel disk
	@run() { timeout 120 $(QEMU) -fw_cfg name=opt/metal/only,string=2 $$1 2>&1 | grep -a "\[rand\]" ; } ; \
	 n=$$(run ""); a=$$(run "-fw_cfg name=opt/metal/noaslr,string=1"); b=$$(run "-fw_cfg name=opt/metal/aslr-seed,string=7"); c=$$(run "-fw_cfg name=opt/metal/aslr-log,string=1"); \
	 echo "$$n" | grep -q "virtio-rng: 32 bytes folded" && ! echo "$$n" | grep -q WARNING \
	 && echo "$$a" | grep -q "WARNING.*randomisation is off" && echo "$$b" | grep -q "WARNING.*seed that was given" && echo "$$c" | grep -q "WARNING.*seed is printed" \
	 && ! echo "$$a$$b$$c" | grep -q "bytes folded" \
	 && w=$$(timeout 120 $(subst -cpu max,-cpu cortex-a72,$(subst file=$(SEED_FILE),string=short,$(QEMU))) -fw_cfg name=opt/metal/only,string=2 2>&1 | grep -a "\[rand\]\|\[kaslr\]") \
	 && echo "$$w" | grep -q "WARNING: neither RNDR nor a full opt/metal/seed" && echo "$$w" | grep -q "kaslr\] kernel image moved.*WARNING: neither RNDR" && ! echo "$$n" | grep -q "neither RNDR" \
	 && echo "== ENTROPY: ALL PASS (virtio-rng folded in on a normal boot; noaslr, aslr-seed and aslr-log warn and skip it; a boot with no RNDR and no seed file warns twice) ==" || { echo "== ENTROPY: FAILED: $$n / $$a / $$b / $$c / $$w =="; exit 1; }

# The boot-time services map their devices and shared pages where the kernel chooses: three boots, the same mappings made
# each time, at different addresses.
service-maps: kernel disk
	@for i in 1 2 3; do timeout 120 $(QEMU) -fw_cfg name=opt/metal/only,string=2 -fw_cfg name=opt/metal/map-log,string=1 > build-map$$i.log 2>&1; done; \
	 r=$$(python3 tools/check_service_maps.py build-map1.log build-map2.log build-map3.log) && echo "== SERVICE MAPS: $$r ==" || { echo "== SERVICE MAPS: $$r =="; exit 1; }

# Kernel address randomisation: the kernel image sits at a different place in RAM on every boot, inside RAM, clear of the
# first 512 KiB, and the system comes up from there (the boot test programs run). Also with the smallest RAM it supports.
KASLR_BOOTS ?= 8
kaslr: kernel disk
	@addrs=""; for i in $$(seq $(KASLR_BOOTS)); do \
	   out=$$(timeout 120 $(QEMU) -fw_cfg name=opt/metal/only,string=2 2>&1); \
	   a=$$(echo "$$out" | grep -a "kernel image moved" | sed 's/.* to \(0x[0-9a-f]*\).*/\1/'); \
	   echo "$$out" | grep -aq "PANIC" && { echo "== KASLR: FAILED: panic at $$a =="; exit 1; }; \
	   echo "$$out" | grep -aq "\[client\] TEST" || { echo "== KASLR: FAILED: no tests ran at $$a =="; exit 1; }; \
	   addrs="$$addrs $$a"; done; \
	 n=$$(echo $$addrs | tr ' ' '\n' | sort -u | grep -c .); \
	 bad=$$(echo $$addrs | tr ' ' '\n' | while read a; do [ -n "$$a" ] && [ $$((a)) -ge $$((0x40080000)) ] && [ $$((a)) -lt $$((0x40000000 + $(RAM_MIB) * 1048576)) ] || echo bad; done | grep -c bad); \
	 small=$$(timeout 120 $(subst -m $(RAM_MIB)M,-m 16M,$(subst $(shell echo $$(( $(RAM_MIB) * 1048576 ))),16777216,$(QEMU))) -fw_cfg name=opt/metal/only,string=2 2>&1 | grep -a "kernel image moved\|no room" | head -1); \
	 [ "$$n" -ge $$(( $(KASLR_BOOTS) - 1 )) ] && [ "$$bad" = 0 ] && echo "$$small" | grep -q . \
	 && echo "== KASLR: ALL PASS ($(KASLR_BOOTS) boots, $$n different kernel addresses, all inside RAM; 16 MiB RAM: $$small) ==" || { echo "== KASLR: FAILED: $$n different addresses in $(KASLR_BOOTS) boots, $$bad out of range:$$addrs; 16 MiB: $$small =="; exit 1; }

# The kernel's stacks have unmapped guard pages: running one out ends in a clear panic (not in silent corruption of
# whatever lay below it), on the boot core and on another core.
stack-guard: kernel disk
	@out=$$(timeout 60 $(QEMU) -fw_cfg name=opt/metal/stack-overflow,string=1 2>&1 | grep -a "stack overflow\|PANIC"); \
	 out1=$$(timeout 60 $(QEMU) -fw_cfg name=opt/metal/stack-overflow-core1,string=1 2>&1 | grep -a "stack overflow\|PANIC"); \
	 outb=$$(timeout 60 $(QEMU) -fw_cfg name=opt/metal/stack-overflow-bigframe,string=1 2>&1 | grep -a "stack overflow\|PANIC"); \
	 echo "$$out" | grep -q "kernel stack overflow on core 0 (guard page hit" && echo "$$out1" | grep -q "kernel stack overflow on core 1 (guard page hit" && echo "$$outb" | grep -q "kernel stack overflow on core 0 (guard page hit" && echo "== STACK GUARD: ALL PASS (core 0 and core 1 report the overflow, a frame bigger than the guard too) ==" || { echo "== STACK GUARD: FAILED: $$out / $$out1 / $$outb =="; exit 1; }

test: host-test erofs-check map-check qemu-test qemu-test-up qemu-test-noncoherent aslr-replay entropy kaslr service-maps stack-guard fuzz

# Licensing hygiene: the ZSTD constants must equal RFC 8878's tables (needs network once).
provenance-check:
	@test -f build-rfc8878.txt || curl -sL https://www.rfc-editor.org/rfc/rfc8878.txt -o build-rfc8878.txt
	python3 tools/check_zstd_tables.py build-rfc8878.txt

# Throw away what runs leave in the project folder: the disk images of `make run-window DISK=runN.img` and of the UI tests, logs and test
# summaries, screenshots and previews. (Close any QEMU window first: its disk image goes too. Build caches stay: `make clean` removes those.)
tidy:
	rm -f run[0-9]*.img ui-*.img gui-*.img disk.img *.log full.sum up.sum build-window.sock build-gui.sock
	rm -rf build-probe

clean:
	cargo clean
	cd kernel && cargo clean
	cd user && cargo clean

# Same boot, but in a QEMU window (serial console shown in the GTK window);
# -no-shutdown keeps the window open after the kernel powers off.
# an interactive run keeps the data partition of the last one (saved colours and settings, installed programs)
run-window: export KEEP_DATA = 1
# (the interactive desktop gets a roomy data partition: a sparse file, the guest uses all of it; the test suites keep the small default)
run-window: export DATA_MIB ?= 1024
run-window: user disk
	cd kernel && cargo build --release --features park-on-exit --target-dir target-window
	qemu-system-aarch64 -machine virt,gic-version=2,iommu=smmuv3 -cpu max -smp $(SMP) -m $(UI_RAM_MIB)M -fw_cfg name=opt/metal/ram-bytes,string=$(shell echo $$(( $(UI_RAM_MIB) * 1048576 ))) \
	  -fw_cfg name=opt/metal/interactive,string=1 -display gtk,zoom-to-fit=on -serial vc:900x600 -monitor none -qmp unix:build-window.sock,server,nowait -semihosting $(QEMU_DEV) \
	  -name "Metal OS" -kernel kernel/target-window/$(TARGET)/release/metal-kernel

# A/B evidence for priority inheritance: on one CPU the polling test must pass with it and fail without.
inherit-ab: user disk
	cd kernel && cargo build --release --features no-inherit --target-dir target-noinherit
	@echo "--- without inheritance (1 CPU):"
	@timeout 60 qemu-system-aarch64 -machine virt,gic-version=2,iommu=smmuv3 -cpu max -smp 1 -m $(RAM_MIB)M -fw_cfg name=opt/metal/ram-bytes,string=$(shell echo $$(( $(RAM_MIB) * 1048576 ))) -nographic -semihosting $(QEMU_DEV) \
	  -kernel kernel/target-noinherit/$(TARGET)/release/metal-kernel 2>&1 | grep "priority inheritance" || true
	@$(MAKE) -s kernel
	@echo "--- with inheritance (1 CPU):"
	@timeout 60 qemu-system-aarch64 -machine virt,gic-version=2,iommu=smmuv3 -cpu max -smp 1 -m $(RAM_MIB)M -fw_cfg name=opt/metal/ram-bytes,string=$(shell echo $$(( $(RAM_MIB) * 1048576 ))) -nographic -semihosting $(QEMU_DEV) \
	  -kernel $(KERNEL_BIN) 2>&1 | grep "priority inheritance" || true
