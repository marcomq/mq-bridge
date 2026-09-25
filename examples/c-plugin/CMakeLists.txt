cmake_minimum_required(VERSION 3.10)
project(mq_bridge_c_plugins C)

set(CMAKE_C_STANDARD 11)
include_directories(${CMAKE_CURRENT_SOURCE_DIR}/../../include)

add_library(drop_heartbeats SHARED minimal.c)
find_package(Threads REQUIRED)
add_library(legacy_payments SHARED plugin.c legacy_parser.c legacy_ledger.c)
target_link_libraries(legacy_payments Threads::Threads)
