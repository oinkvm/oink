#include "Authorization.h"
#include <cassert>
#include <iostream>

int main() {
    using admission::CallerToken;
    // A SYSTEM client without impersonation still has a SYSTEM effective token.
    if (!admission::effective_system(CallerToken::absent, [] { return true; })) {
        std::cerr << "SYSTEM process without impersonation lost worker admission\n";
        return 1;
    }
    assert(!admission::effective_system(CallerToken::absent, [] { return false; }));
    unsigned process_reads = 0;
    const auto system_process = [&] { ++process_reads; return true; };
    assert(admission::effective_system(CallerToken::system, system_process));
    assert(!admission::effective_system(CallerToken::other, system_process));
    assert(!admission::effective_system(CallerToken::unreadable, system_process));
    assert(process_reads == 0); // Never borrow SYSTEM while impersonating a user.
    // Admission depends only on the protected package tuple, never where the
    // SYSTEM worker copy lives (transactions/<tx>/worker.exe in production).
    const std::string valid = R"({"schema":1,"product_code":"{c4178f6a-81f8-5e1d-9920-232ae29d1d4f}","package_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","remove_product_code":"","remove_package_sha256":""})";
    const std::string code = "c4178f6a-81f8-5e1d-9920-232ae29d1d4f", hash(64, 'b');
    admission::Object object;
    assert(admission::parse(valid, object) && admission::worker_shape(object));
    object["remove_product_code"] = {"0", true};
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    assert(admission::matches(object, code, hash, "", ""));
    assert(!admission::matches(object, code, std::string(64, 'c'), "", ""));
    assert(!admission::matches(object, "d4178f6a-81f8-5e1d-9920-232ae29d1d4f", hash, "", ""));
    object["schema"].text = "2";
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    object["worker_pid"] = {"1234", true};
    assert(!admission::worker_shape(object));
    assert(admission::parse(valid, object));
    const std::string old_code = "d4178f6a-81f8-5e1d-9920-232ae29d1d4f", old_hash(64, 'c');
    object["remove_product_code"].text = old_code;
    assert(!admission::worker_shape(object));
    object["remove_package_sha256"].text = old_hash;
    assert(admission::worker_shape(object));
    assert(admission::matches(object, code, hash, old_code, old_hash));
    assert(!admission::matches(object, code, hash, old_code, hash));
    assert(admission::matches(object, old_code, old_hash, "", ""));
    assert(!admission::matches(object, old_code, hash, "", ""));
    object.erase("remove_product_code"); object["unknown"] = {"", false};
    assert(!admission::worker_shape(object));
    assert(!admission::parse("{\"schema\":1,\"schema\":1}", object));
    assert(!admission::parse("{\"schema\":01}", object));
    assert(!admission::parse("{\"path\":\"..\\\\secret\"}", object));
    assert(!admission::parse(valid + "garbage", object));
    assert(!admission::parse(std::string(8193, ' '), object));
    assert(admission::guid("{C4178F6A-81F8-5E1D-9920-232AE29D1D4F}") == "c4178f6a-81f8-5e1d-9920-232ae29d1d4f");
    assert(admission::guid("../../x").empty());
    std::cout << "authorization_checks=28\n";
}
