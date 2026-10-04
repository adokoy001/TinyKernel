#!/bin/sh
# Unit tests of the pure kernel modules, compiled for and run on the host.
set -eu
cd "$(dirname "$0")/.."
mkdir -p build
for module in shell frames sched mac fs inet pci rtl8139 shell_lang operations records editor variables executable usermem handles user_abi; do
    rustc --edition=2021 --test "src/$module.rs" -o "build/$module-tests"
    "./build/$module-tests" -q
done
rustc --edition=2021 --test tests/host_resources.rs -o build/resources-tests
./build/resources-tests -q
rustc --edition=2021 --test tests/host_netstack.rs -o build/netstack-tests
./build/netstack-tests -q
rustc --edition=2021 --test tests/host_plans.rs -o build/plans-tests
./build/plans-tests -q
# Compile without --test so the production AddressSpace implementation is
# exercised against the harness's aligned host allocator and paging stub.
rustc --edition=2021 tests/host_usermem.rs -o build/usermem-owned-tests
./build/usermem-owned-tests

rustc --edition=2021 --test tests/host_storage.rs -o build/storage-tests
./build/storage-tests -q

rustc --edition=2021 --test tests/process_control.rs -o build/process-control-tests
./build/process-control-tests -q
rustc --edition=2021 --test tests/fs_crash.rs -o build/fs-crash-tests
./build/fs-crash-tests -q

python3 tests/build_guard.py
