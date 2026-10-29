#include "../src/Loopback.hpp"
#include <cassert>
#include <thread>
int main() {
    Loopback ring;
    assert(ring.read(0) == 0);
    for (uint64_t i = 0; i < 1000; ++i) ring.write(i, float(i));
    for (int client = 0; client < 3; ++client)
        for (uint64_t i = 0; i < 1000; ++i) assert(ring.read(i) == float(i));
    ring.write(Loopback::Capacity + 1, -1);
    assert(ring.read(1) == 0);
    assert(ring.read(Loopback::Capacity + 1) == -1);
    ring.clear();
    assert(ring.read(Loopback::Capacity + 1) == 0);
    // Readers may race overwrite: silence or the matching frame, never another frame.
    std::thread writer([&] { for (uint64_t i = 0; i < 200000; ++i) ring.write(i, float(i)); });
    for (uint64_t i = 0; i < 200000; ++i) {
        const auto value = ring.read(i);
        assert(value == 0 || value == float(i));
    }
    writer.join();
}
