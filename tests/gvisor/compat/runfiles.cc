// Replaces gVisor's test/util/test_util_runfiles.cc, which finds the helper programs a test runs
// (exec workloads and the like) through Bazel's runfiles library: scripts/build-gvisor-tests
// installs every program, tests and helpers alike, side by side in one directory.
#include <string>

#include "test/util/fs_util.h"
#include "test/util/test_util.h"

namespace gvisor {
namespace testing {

std::string RunfilePath(std::string path) {
  return JoinPath("/usr/lib/gvisor-tests/bin", std::string(Basename(path)));
}

}  // namespace testing
}  // namespace gvisor
