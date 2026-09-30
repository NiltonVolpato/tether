# tether's integration test app: the C++ core on an ESP32 (or QEMU), talking
# to the Rust core on the host over UART1. See README.md.

cmake_minimum_required(VERSION 3.22)

set(EXTRA_COMPONENT_DIRS ${CMAKE_CURRENT_LIST_DIR}/../tether)
set(COMPONENTS main)
# Generated config belongs with the build, not the sources.
set(SDKCONFIG ${CMAKE_BINARY_DIR}/sdkconfig)

include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(tether_test_app)
