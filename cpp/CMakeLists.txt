# Host build of tether's C++ core, with its tests. The component itself is
# tether/, which is all that ships.

cmake_minimum_required(VERSION 3.22)
# Name the macOS SDK, so compile_commands.json carries -isysroot and clangd and
# clang-tidy from a non-Apple LLVM find the standard headers.
set(CMAKE_OSX_SYSROOT macosx)
project(tether_host LANGUAGES CXX)

set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
# No exceptions or RTTI, as on the device, in the tests and the libraries they
# use too: mixing them with code that has either can fail to link (typeinfo
# the library didn't emit) or misbehave.
add_compile_options(-fno-exceptions -fno-rtti)

add_subdirectory(tether)

enable_testing()
add_subdirectory(tests)
