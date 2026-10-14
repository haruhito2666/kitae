cmake_minimum_required(VERSION 3.20)
project(kitae_native LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

find_package(PkgConfig REQUIRED)
pkg_check_modules(SDL2 REQUIRED sdl2)
pkg_check_modules(SDL2_TTF REQUIRED SDL2_ttf)

add_library(kitae_renderer STATIC src/kitae_renderer.cpp)

# The Rust build (cmake crate) drives `--target install`; place the archive at
# the install root, where the Rust link search path points.
install(TARGETS kitae_renderer ARCHIVE DESTINATION .)

target_include_directories(kitae_renderer PUBLIC
    ${CMAKE_CURRENT_SOURCE_DIR}/include
    ${SDL2_INCLUDE_DIRS}
    ${SDL2_TTF_INCLUDE_DIRS}
)

target_compile_options(kitae_renderer PRIVATE ${SDL2_CFLAGS_OTHER} ${SDL2_TTF_CFLAGS_OTHER})
