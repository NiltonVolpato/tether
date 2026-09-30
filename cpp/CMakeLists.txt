# The C++ core of the RPC framework: an ESP-IDF component, or a plain CMake
# library (with its tests) everywhere else.

cmake_minimum_required(VERSION 3.22)

set(RPC_SOURCES src/cobs.cpp src/frame.cpp)
set(RPC_INCLUDE_DIRS include generated third_party/flatbuffers/include)

if(ESP_PLATFORM)
  # IDF compiles C++ as gnu++26, without exceptions or RTTI.
  idf_component_register(SRCS ${RPC_SOURCES} INCLUDE_DIRS ${RPC_INCLUDE_DIRS})
  return()
endif()

project(rpc LANGUAGES CXX)

add_library(rpc ${RPC_SOURCES})
target_include_directories(rpc PUBLIC include)
# Generated and third-party headers are compiled without our warnings.
target_include_directories(rpc SYSTEM PUBLIC generated
                                             third_party/flatbuffers/include)
target_compile_features(rpc PUBLIC cxx_std_23)
# The same restrictions as on the device.
target_compile_options(rpc PRIVATE -Wall -Wextra -Wpedantic -Werror
                                   -fno-exceptions -fno-rtti)

if(PROJECT_IS_TOP_LEVEL)
  set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
  enable_testing()
  add_subdirectory(tests)
endif()
