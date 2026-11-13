LOCAL_PATH := $(call my-dir)
include $(CLEAR_VARS)
LOCAL_MODULE := oink_android_audio
LOCAL_SRC_FILES := audio.cpp forward.cpp
LOCAL_CPP_FEATURES := exceptions
LOCAL_CPPFLAGS := -std=c++17 -Wall -Wextra -Werror
LOCAL_C_INCLUDES := $(LOCAL_PATH)/../../../../../../../crates/oink-mobile-ffi/include
LOCAL_LDLIBS := -laaudio -llog -L$(LOCAL_PATH)/../jniLibs/$(TARGET_ARCH_ABI) -loink_mobile_ffi
include $(BUILD_SHARED_LIBRARY)
