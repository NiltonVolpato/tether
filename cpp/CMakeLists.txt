# Host build of tether's C++ core, with its tests. The component itself is
# tether/, which is all that ships.

cmake_minimum_required(VERSION 3.22)
# Name the macOS SDK, so compile_commands.json carries -isysroot and clangd and
# clang-tidy from a non-Apple LLVM find the standard headers.
set(CMAKE_OSX_SYSROOT macosx)
project(tether_host LANGUAGES CXX)

set(CMAKE_EXPORT_COMPILE_COMMANDS ON)

add_subdirectory(tether)

enable_testing()
add_subdirectory(tests)
