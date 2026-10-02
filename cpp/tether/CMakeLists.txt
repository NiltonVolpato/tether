# tether's C++ core: an ESP-IDF component, or a plain CMake library everywhere
# else (see ../CMakeLists.txt for the host build with tests).

cmake_minimum_required(VERSION 3.22)

set(TETHER_SOURCES src/cobs.cpp src/frame.cpp src/link.cpp src/server.cpp
                   src/router.cpp)
set(TETHER_INCLUDE_DIRS include generated third_party/flatbuffers/include)

if(ESP_PLATFORM)
  # IDF compiles C++ as gnu++26, without exceptions or RTTI.
  idf_component_register(SRCS ${TETHER_SOURCES} INCLUDE_DIRS
                         ${TETHER_INCLUDE_DIRS})
  return()
endif()

add_library(tether ${TETHER_SOURCES})
target_include_directories(tether PUBLIC include)
# Generated and third-party headers are compiled without our warnings.
target_include_directories(tether SYSTEM PUBLIC generated
                                                third_party/flatbuffers/include)
target_compile_features(tether PUBLIC cxx_std_23)
# The same restrictions as on the device.
target_compile_options(tether PRIVATE -Wall -Wextra -Wpedantic -Werror
                                      -fno-exceptions -fno-rtti)
