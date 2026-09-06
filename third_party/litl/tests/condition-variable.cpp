/* SPDX-License-Identifier: MIT */
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <mutex>
#include <thread>

int main() {
    using namespace std::chrono_literals;
    std::mutex mutex;
    std::condition_variable cv;
    bool go = false, done = false;
    std::thread worker([&] {
        std::unique_lock lock(mutex);
        cv.wait(lock, [&] { return go; });
        done = true;
        cv.notify_one();
    });
    {
        std::unique_lock lock(mutex);
        if (cv.wait_for(lock, 2ms) != std::cv_status::timeout)
            std::abort();
        go = true;
        cv.notify_one();
        if (!cv.wait_for(lock, 2s, [&] { return done; }))
            std::abort();
        if (cv.wait_until(lock, std::chrono::system_clock::now() + 2ms) !=
            std::cv_status::timeout)
            std::abort();
    }
    worker.join();
    std::puts("PASS std::condition_variable wait, wait_for, wait_until, notification");
}
