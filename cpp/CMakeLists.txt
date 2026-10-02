# Host build of tether's C++ core, with its tests. The component itself is
# tether/, which is all that ships.

cmake_minimum_required(VERSION 3.22)
# Name the macOS SDK, so compile_commands.json carries -isysroot and clangd and
# clang-tidy from a non-Apple LLVM find the standard headers.
set(CMAKE_OSX_SYSROOT macosx)
project(tether_host LANGUAGES CXX)

set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
# No RTTI, as on the device, in the tests and GoogleTest too: a class with
# virtual functions defined in the library has no typeinfo to give a subclass
# that has RTTI.
add_compile_options(-fno-rtti)

add_subdirectory(tether)

enable_testing()
add_subdirectory(tests)
