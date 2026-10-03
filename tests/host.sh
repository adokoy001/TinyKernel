#!/bin/sh
# Unit tests of the pure kernel modules, compiled for and run on the host.
set -eu
cd "$(dirname "$0")/.."
mkdir -p build
for module in shell frames sched mac fs inet pci rtl8139; do
    rustc --edition=2021 --test "src/$module.rs" -o "build/$module-tests"
    "./build/$module-tests" -q
done
rustc --edition=2021 --test tests/host_resources.rs -o build/resources-tests
./build/resources-tests -q
rustc --edition=2021 --test tests/host_netstack.rs -o build/netstack-tests
./build/netstack-tests -q
