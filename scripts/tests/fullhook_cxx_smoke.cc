// SPDX-License-Identifier: GPL-2.0-only
// The libstdc++ threading primitives that LevelDB's port layer is built from:
// std::mutex is a PTHREAD_MUTEX_INITIALIZER object, std::condition_variable a
// PTHREAD_COND_INITIALIZER one, and std::recursive_mutex a
// PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP one, all routed through the
// interposer without an init call. The C smoke covers everything else.
#include <dlfcn.h>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <thread>

namespace {

constexpr unsigned kRounds = 1000;

std::mutex mutex;
std::condition_variable changed;
std::recursive_mutex recursive;
unsigned turn;

void Require(bool condition, const char *message) {
  if (!condition) {
    std::fprintf(stderr, "fullhook C++ smoke failed: %s\n", message);
    std::exit(1);
  }
}

void CheckInterposed(const char *name) {
  void *address = dlsym(RTLD_DEFAULT, name);
  Dl_info info;
  Require(address && dladdr(address, &info) && info.dli_fname &&
              std::strstr(info.dli_fname, "fullhook"),
          name);
}

// Hand the turn back and forth so every round blocks on the condition variable
// and reacquires the mutex through the hook.
void PassTurn(unsigned mine) {
  for (unsigned round = 0; round < kRounds; ++round) {
    std::unique_lock<std::mutex> lock(mutex);
    changed.wait(lock, [mine] { return turn == mine; });
    turn ^= 1;
    changed.notify_all();
  }
}

}  // namespace

int main() {
  CheckInterposed("pthread_mutex_lock");
  CheckInterposed("pthread_cond_wait");

  {
    std::lock_guard<std::recursive_mutex> outer(recursive);
    std::lock_guard<std::recursive_mutex> inner(recursive);
    Require(recursive.try_lock(), "recursive re-entry through try_lock");
    recursive.unlock();
  }
  Require(recursive.try_lock(), "recursive mutex released after the last hold");
  recursive.unlock();

  std::thread other(PassTurn, 1);
  PassTurn(0);
  other.join();
  Require(turn == 0, "handshake left the turn with the first thread");

  std::printf("fullhook C++ smoke ok: rounds=%u\n", kRounds);
  return 0;
}
