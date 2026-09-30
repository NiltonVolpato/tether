include(FetchContent)
FetchContent_Declare(
  googletest
  URL https://github.com/google/googletest/archive/refs/tags/v1.17.0.tar.gz
  FIND_PACKAGE_ARGS NAMES GTest)
FetchContent_Declare(
  nlohmann_json
  URL https://github.com/nlohmann/json/releases/download/v3.12.0/json.tar.xz
  FIND_PACKAGE_ARGS)
FetchContent_MakeAvailable(googletest nlohmann_json)

add_executable(rpc_tests cobs_test.cpp crc32_test.cpp frame_test.cpp
                         golden_test.cpp)
target_link_libraries(rpc_tests PRIVATE rpc GTest::gmock_main
                                        nlohmann_json::nlohmann_json)
target_compile_options(rpc_tests PRIVATE -Wall -Wextra -Werror)
target_compile_definitions(
  rpc_tests
  PRIVATE
    RPC_GOLDEN_FRAMES="${CMAKE_CURRENT_SOURCE_DIR}/../../golden/frames.json")

include(GoogleTest)
gtest_discover_tests(rpc_tests)
