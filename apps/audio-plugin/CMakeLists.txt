cmake_minimum_required(VERSION 3.24)
project(OinkMicrophone LANGUAGES CXX)
if(NOT APPLE)
  message(FATAL_ERROR "The HAL plugin requires macOS")
endif()
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_OSX_DEPLOYMENT_TARGET "14.2")
include(FetchContent)
FetchContent_Declare(aspl
  URL https://codeload.github.com/gavv/libASPL/tar.gz/633e0f70203edd87d320fc5a3cae901e1363aac5
  URL_HASH SHA256=8583fb549419eab14b5c636ee4eb1f0b1df74c534eaf2f990d8bb0fb5de6e01b)
# Upstream CMake derives version from a git checkout. Build its checked-in,
# generated C++ directly, without regenerating files or downloading test libraries.
FetchContent_GetProperties(aspl)
if(NOT aspl_POPULATED)
  FetchContent_Populate(aspl)
endif()
file(GLOB ASPL_SOURCES CONFIGURE_DEPENDS "${aspl_SOURCE_DIR}/src/*.cpp")
add_library(aspl STATIC ${ASPL_SOURCES})
target_include_directories(aspl SYSTEM PUBLIC "${aspl_SOURCE_DIR}/include")
target_compile_options(aspl PRIVATE -g -Wno-deprecated-declarations
  "-ffile-prefix-map=${CMAKE_CURRENT_SOURCE_DIR}=oink/audio-plugin"
  "-ffile-prefix-map=${CMAKE_BINARY_DIR}=oink/audio-plugin/build")
target_link_libraries(aspl PUBLIC "-framework CoreFoundation")
if(NOT DEFINED OINK_PLUGIN_GIT_SHA)
  message(FATAL_ERROR "Provide the package source-tree SHA256 as OINK_PLUGIN_GIT_SHA")
endif()
# A driver-only identity: unrelated application commits and signing timestamps
# are provenance, not reasons to reinstall HAL. Include pinned dependency hashes,
# driver sources and ABI/code-generation inputs; exclude probes/docs/installers.
file(GLOB_RECURSE DRIVER_INPUTS CONFIGURE_DEPENDS RELATIVE "${CMAKE_CURRENT_SOURCE_DIR}" "src/*")
list(APPEND DRIVER_INPUTS CMakeLists.txt Info.plist.in)
list(SORT DRIVER_INPUTS)
set(DRIVER_FINGERPRINT "mic-v1|${CMAKE_OSX_ARCHITECTURES}|${CMAKE_OSX_DEPLOYMENT_TARGET}|${CMAKE_BUILD_TYPE}|${CMAKE_CXX_COMPILER_ID}|${CMAKE_CXX_COMPILER_VERSION}|${CMAKE_CXX_FLAGS}|${CMAKE_CXX_FLAGS_RELEASE}")
execute_process(COMMAND xcrun --sdk macosx --show-sdk-version
  OUTPUT_VARIABLE DRIVER_SDK OUTPUT_STRIP_TRAILING_WHITESPACE COMMAND_ERROR_IS_FATAL ANY)
string(APPEND DRIVER_FINGERPRINT "|sdk=${DRIVER_SDK}")
foreach(INPUT IN LISTS DRIVER_INPUTS)
  file(SHA256 "${CMAKE_CURRENT_SOURCE_DIR}/${INPUT}" INPUT_HASH)
  string(APPEND DRIVER_FINGERPRINT "|${INPUT}=${INPUT_HASH}")
endforeach()
string(SHA256 OINK_PLUGIN_VERSION "${DRIVER_FINGERPRINT}")
add_library(OinkMicrophone MODULE src/Driver.cpp)
target_compile_definitions(OinkMicrophone PRIVATE OINK_PLUGIN_GIT_SHA="${OINK_PLUGIN_GIT_SHA}" OINK_PLUGIN_VERSION="${OINK_PLUGIN_VERSION}")
target_link_libraries(OinkMicrophone PRIVATE aspl "-framework CoreAudio")
target_compile_options(OinkMicrophone PRIVATE -g -Wall -Wextra -Werror
  "-ffile-prefix-map=${CMAKE_CURRENT_SOURCE_DIR}=oink/audio-plugin"
  "-ffile-prefix-map=${CMAKE_BINARY_DIR}=oink/audio-plugin/build")
set_target_properties(OinkMicrophone PROPERTIES
  BUNDLE TRUE BUNDLE_EXTENSION driver PREFIX "" SUFFIX ""
  MACOSX_BUNDLE_INFO_PLIST "${CMAKE_CURRENT_SOURCE_DIR}/Info.plist.in")
add_custom_command(TARGET OinkMicrophone POST_BUILD
  COMMAND ${CMAKE_COMMAND} -E copy_directory "${CMAKE_CURRENT_SOURCE_DIR}/licenses"
    "$<TARGET_BUNDLE_DIR:OinkMicrophone>/Contents/Resources/licenses")
enable_testing()
add_executable(loopback-test tests/loopback.cpp)
target_compile_options(loopback-test PRIVATE -Wall -Wextra -Werror -UNDEBUG)
add_test(NAME loopback COMMAND loopback-test)
# Exercise the real built driver's property ABI without installing or starting IO.
add_executable(driver-identity-test tests/driver-identity.cpp)
target_compile_options(driver-identity-test PRIVATE -Wall -Wextra -Werror -UNDEBUG)
target_link_libraries(driver-identity-test PRIVATE "-framework CoreFoundation")
add_dependencies(driver-identity-test OinkMicrophone)
add_test(NAME driver-identity COMMAND driver-identity-test
  "$<TARGET_FILE:OinkMicrophone>" "${OINK_PLUGIN_VERSION}" "${OINK_PLUGIN_GIT_SHA}")

# Verify the actual clone/copy/chmod commands without root or HAL mutations.
find_package(Python3 REQUIRED COMPONENTS Interpreter)
add_test(NAME install-permissions COMMAND "${Python3_EXECUTABLE}"
  "${CMAKE_CURRENT_SOURCE_DIR}/tests/install-permissions.py")
