cmake_minimum_required(VERSION 3.16)
project(fcitx5-xaidict VERSION 0.2.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(Fcitx5Core REQUIRED)
find_package(Fcitx5Module REQUIRED COMPONENTS DBus)
include("${FCITX_INSTALL_CMAKECONFIG_DIR}/Fcitx5Package/Fcitx5CompilerSettings.cmake" OPTIONAL)
include("${FCITX_INSTALL_CMAKECONFIG_DIR}/Fcitx5Utils/Fcitx5CompilerSettings.cmake" OPTIONAL)

add_library(xaidict MODULE xaidict.cpp)
target_link_libraries(xaidict Fcitx5::Core Fcitx5::Module::DBus)
target_include_directories(xaidict PRIVATE
  /usr/include/Fcitx5/Module
  /usr/include/Fcitx5/Module/fcitx-module/dbus
  /usr/include/Fcitx5/Core
  /usr/include/Fcitx5/Utils
  /usr/include/Fcitx5/Config
)
set_target_properties(xaidict PROPERTIES PREFIX "lib" OUTPUT_NAME "xaidict")

if(NOT DEFINED FCITX_INSTALL_ADDONDIR)
  set(FCITX_INSTALL_ADDONDIR "${CMAKE_INSTALL_PREFIX}/lib/fcitx5")
endif()
if(NOT DEFINED FCITX_INSTALL_PKGDATADIR)
  set(FCITX_INSTALL_PKGDATADIR "${CMAKE_INSTALL_PREFIX}/share/fcitx5")
endif()

install(TARGETS xaidict DESTINATION "${FCITX_INSTALL_ADDONDIR}")
install(FILES xaidict.conf DESTINATION "${FCITX_INSTALL_PKGDATADIR}/addon")
# No inputmethod/*.conf — this is a Module, not a replace-pinyin IM.
