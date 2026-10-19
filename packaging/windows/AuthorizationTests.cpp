#include "Authorization.h"
#include <cassert>
#include <iostream>

int main() {
    const std::string valid = R"({"schema":1,"transaction":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","worker_pid":1234,"worker_created_filetime":"0123456789abcdef","worker_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","lock_handle":"0000000000001234","operation":"install","product_code":"{c4178f6a-81f8-5e1d-9920-232ae29d1d4f}","package_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","remove_product_code":"","remove_package_sha256":"","pending_sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"})";
    admission::Object object;
    assert(admission::parse(valid, object) && admission::worker_shape(object));
    object["worker_pid"].text = "4294967296";
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    object["worker_pid"].number = false;
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    object["lock_handle"].text = "0000000000000000";
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    object["operation"].text = "uninstall";
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    object.erase("remove_product_code"); object["unknown"] = {"", false};
    assert(!admission::worker_shape(object));
    assert(!admission::parse("{\"schema\":1,\"schema\":1}", object));
    assert(!admission::parse("{\"schema\":01}", object));
    assert(!admission::parse("{\"path\":\"..\\\\secret\"}", object));
    assert(!admission::parse(valid + "garbage", object));
    assert(!admission::parse(std::string(8193, ' '), object));
    assert(admission::guid("{C4178F6A-81F8-5E1D-9920-232AE29D1D4F}") == "c4178f6a-81f8-5e1d-9920-232ae29d1d4f");
    assert(admission::guid("../../x").empty());
    std::cout << "authorization_checks=13\n";
}
