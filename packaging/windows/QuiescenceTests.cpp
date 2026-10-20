#include "Quiescence.h"
#include <cassert>
#include <iostream>

int main() {
    const std::wstring directory = L"C:\\Program Files\\Oink";
    const auto roles = {L"oink.exe", L"oink-agent.exe", L"oink-service.exe", L"oink-crash-collector.exe"};
    for (const auto role : roles) assert(quiescence::product_path(directory, directory + L"\\" + role, roles));
    assert(quiescence::product_path(directory, directory + L"\\OINK.EXE", roles));
    assert(!quiescence::product_path(directory, directory + L"\\oink-update-worker.exe", roles));
    assert(!quiescence::product_path(directory, directory + L"\\maintenance\\oink.exe", roles));
    assert(!quiescence::product_path(directory, L"C:\\Other\\oink.exe", roles));
    std::cout << "post_stop_product_role_checks=8\n";
}
