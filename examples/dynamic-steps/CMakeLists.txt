cmake_minimum_required(VERSION 3.16)
project(shapes C)

# A generated header: written by the build (a custom command), so the
# compile steps that include it must run after it.
add_custom_command(
  OUTPUT ${CMAKE_CURRENT_BINARY_DIR}/shapes_version.h
  COMMAND ${CMAKE_COMMAND} -DOUT=${CMAKE_CURRENT_BINARY_DIR}/shapes_version.h
          -P ${CMAKE_CURRENT_SOURCE_DIR}/version.cmake
  DEPENDS ${CMAKE_CURRENT_SOURCE_DIR}/version.cmake)
add_custom_target(shapes_version DEPENDS ${CMAKE_CURRENT_BINARY_DIR}/shapes_version.h)

add_library(shapes SHARED src/circle.c src/square.c src/triangle.c src/hexagon.c src/util.c)
target_include_directories(shapes PUBLIC include ${CMAKE_CURRENT_BINARY_DIR})
add_dependencies(shapes shapes_version)

add_executable(shapes-info src/main.c)
target_link_libraries(shapes-info PRIVATE shapes m)

install(TARGETS shapes shapes-info)
install(FILES include/shapes.h DESTINATION include)
