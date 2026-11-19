#pragma once
// Elevated recovery is confined to the fixed product tree. Seal each directory
// through a no-delete-share, no-reparse handle before inspecting its children;
// never recurse through a caller's path or trust records after widening an ACL.
#include "RecoveryPolicy.h"
#include "RecoveryTree.h"
#include <taskschd.h>
#include "MaintenanceCleanup.h"

namespace recovery {
inline int failure_code = 5;
inline constexpr auto public_acl = L"O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;GRGX;;;BU)";
inline constexpr auto product_acl = L"O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)";
inline constexpr auto private_acl = L"O:SYG:SYD:P(A;OICI;FA;;;SY)";

inline bool privilege(const wchar_t* name) {
    HANDLE token = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &token)) return false;
    TOKEN_PRIVILEGES value{}; value.PrivilegeCount = 1;
    bool ok = LookupPrivilegeValueW(nullptr, name, &value.Privileges[0].Luid) != FALSE;
    value.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
    SetLastError(ERROR_SUCCESS);
    ok = ok && AdjustTokenPrivileges(token, FALSE, &value, 0, nullptr, nullptr) && GetLastError() == ERROR_SUCCESS;
    CloseHandle(token);
    return ok;
}
inline bool set_acl(HANDLE handle, const wchar_t* text) {
    PSECURITY_DESCRIPTOR descriptor = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(text, SDDL_REVISION_1, &descriptor, nullptr)) return false;
    PSID owner = nullptr, group = nullptr; PACL acl = nullptr; BOOL present = FALSE, defaulted = FALSE;
    bool ok = GetSecurityDescriptorOwner(descriptor, &owner, &defaulted) &&
        GetSecurityDescriptorGroup(descriptor, &group, &defaulted) &&
        GetSecurityDescriptorDacl(descriptor, &present, &acl, &defaulted) && present && acl &&
        SetSecurityInfo(handle, SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION |
            PROTECTED_DACL_SECURITY_INFORMATION, owner, group, acl, nullptr) == ERROR_SUCCESS;
    LocalFree(descriptor);
    return ok;
}
inline bool transaction_image(const std::wstring& image) {
    const auto prefix = maintenance::directory() + L"\\transactions\\";
    return image.size() > prefix.size() && _wcsnicmp(image.c_str(), prefix.c_str(), prefix.size()) == 0;
}
inline bool workers(bool stop, bool& alive) {
    alive = false;
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE) return false;
    PROCESSENTRY32W entry{sizeof(entry)};
    BOOL more = Process32FirstW(snapshot, &entry); bool ok = true;
    while (more) {
        HANDLE process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, entry.th32ProcessID);
        if (process) {
            if (WaitForSingleObject(process, 0) == WAIT_TIMEOUT && transaction_image(maintenance::image(process))) {
                alive = true;
                if (stop) {
                    // Reopen only the same creation object; PID reuse cannot
                    // turn cleanup into termination of an unrelated process.
                    HANDLE terminate = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, FALSE, entry.th32ProcessID);
                    ok = terminate && maintenance::image(terminate) == maintenance::image(process) &&
                        WaitForSingleObject(process, 0) == WAIT_TIMEOUT && TerminateProcess(terminate, ERROR_CANCELLED) &&
                        WaitForSingleObject(terminate, 15000) == WAIT_OBJECT_0;
                    if (terminate) CloseHandle(terminate);
                }
            }
            CloseHandle(process);
        }
        if (!ok) break;
        more = Process32NextW(snapshot, &entry);
    }
    if (ok && !more && GetLastError() != ERROR_NO_MORE_FILES) ok = false;
    CloseHandle(snapshot); return ok;
}
inline bool reset_tasks(MSIHANDLE install) {
    const HRESULT initialized = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(initialized) && initialized != RPC_E_CHANGED_MODE) return false;
    ITaskService* service = nullptr; ITaskFolder* folder = nullptr; IRegisteredTaskCollection* tasks = nullptr;
    HRESULT result = CoCreateInstance(__uuidof(TaskScheduler), nullptr, CLSCTX_INPROC_SERVER,
        __uuidof(ITaskService), reinterpret_cast<void**>(&service));
    VARIANT empty; VariantInit(&empty);
    if (SUCCEEDED(result)) result = service->Connect(empty, empty, empty, empty);
    BSTR root_folder = SysAllocString(L"\\");
    if (SUCCEEDED(result)) result = service->GetFolder(root_folder, &folder);
    SysFreeString(root_folder);
    if (SUCCEEDED(result)) result = folder->GetTasks(TASK_ENUM_HIDDEN, &tasks);
    LONG count = 0;
    if (SUCCEEDED(result)) result = tasks->get_Count(&count);
    const std::wstring prefix = L"OinkUpdate-";
    std::vector<std::wstring> names;
    for (LONG index = 1; SUCCEEDED(result) && index <= count; ++index) {
        VARIANT item; VariantInit(&item); item.vt = VT_I4; item.lVal = index;
        IRegisteredTask* task = nullptr; BSTR name = nullptr;
        result = tasks->get_Item(item, &task);
        if (SUCCEEDED(result)) result = task->get_Name(&name);
        if (SUCCEEDED(result)) {
            const std::wstring value(name);
            // Include orphan tasks whose transaction directory was already
            // removed. This fixed namespace is reserved to Oink, never executed
            // during recovery and never derived from an untrusted record.
            if (value.rfind(prefix, 0) == 0 && admission::hex(maintenance::narrow(value.substr(prefix.size())), 32)) {
                names.push_back(value);
            }
        }
        if (name) SysFreeString(name);
        if (task) task->Release();
    }
    for (const auto& value : names) {
        if (FAILED(result)) break;
        BSTR name = SysAllocString(value.c_str());
        result = folder->DeleteTask(name, 0);
        SysFreeString(name);
        if (result == HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)) result = S_OK;
        diagnostic(install, L"Recovery removed transaction task: " + value + L", result=" + std::to_wstring(result));
    }
    if (tasks) tasks->Release();
    if (folder) folder->Release();
    if (service) service->Release();
    if (SUCCEEDED(initialized)) CoUninitialize();
    if (FAILED(result)) diagnostic(install, L"Recovery could not clean the transaction task namespace, HRESULT=" + std::to_wstring(result));
    return SUCCEEDED(result);
}

struct Tree {
    bool widened = false;
    unsigned checked = 0;
    MSIHANDLE install;
    bool visit(const std::wstring& path, bool maintenance_tree, bool top = false) {
        HANDLE handle = CreateFileW(path.c_str(), READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ, nullptr, OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
        if (handle == INVALID_HANDLE_VALUE) return false;
        BY_HANDLE_FILE_INFORMATION info{};
        bool ok = GetFileInformationByHandle(handle, &info) && !(info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) &&
            ((info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || info.nNumberOfLinks == 1) && opened_as(handle, path);
        if (!ok) {
            diagnostic(install, L"Recovery refused redirected, multiply-linked or noncanonical path: " + path);
            CloseHandle(handle); return false;
        }
        ++checked;
        const bool safe = protected_handle(handle);
        widened = widened || !safe;
        if (!safe) diagnostic(install, L"Recovery discards untrusted maintenance state after repairing: " + path);
        const auto acl = maintenance_tree && !top ? private_acl : top ? public_acl : product_acl;
        // Also give existing maintenance roots Users list/read, without inheriting
        // that grant into SYSTEM-only staging trees (Explorer's Continue trap).
        ok = set_acl(handle, acl);
        if (ok && (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY)) {
            WIN32_FIND_DATAW entry{};
            HANDLE search = FindFirstFileW((path + L"\\*").c_str(), &entry);
            if (search == INVALID_HANDLE_VALUE) ok = GetLastError() == ERROR_FILE_NOT_FOUND;
            else {
                do {
                    if (wcscmp(entry.cFileName, L".") == 0 || wcscmp(entry.cFileName, L"..") == 0) continue;
                    if (!literal_entry(entry.cFileName)) {
                        diagnostic(install, L"Recovery refused ambiguous Win32 entry: " + path + L"\\" + entry.cFileName);
                        ok = false; break;
                    }
                    const auto child = path + L"\\" + entry.cFileName;
                    if (_wcsicmp(child.c_str(), maintenance::lock_path().c_str()) == 0) continue;
                    const bool is_maintenance = _wcsicmp(child.c_str(), maintenance::directory().c_str()) == 0;
                    if (!visit(child, maintenance_tree || is_maintenance, is_maintenance)) { ok = false; break; }
                } while (FindNextFileW(search, &entry));
                if (ok && GetLastError() != ERROR_NO_MORE_FILES) ok = false;
                FindClose(search);
            }
        }
        CloseHandle(handle); return ok;
    }
};

struct Parents {
    std::vector<HANDLE> handles;
    ~Parents() { for (auto handle : handles) CloseHandle(handle); }
};
inline bool pin_ancestors(MSIHANDLE install, Parents& parents) {
    auto parent = root();
    for (;;) {
        const auto slash = parent.find_last_of(L"\\");
        if (slash == std::wstring::npos) break;
        const bool volume = slash == 2 && parent[1] == L':';
        parent.resize(volume ? 3 : slash);
        Boundary pinned;
        if (!protected_path(parent) || !pinned.pin(parent)) {
            failure_code = 4; diagnostic(install, L"Recovery refused unsafe parent (not owned by Oink): " + parent); return false;
        }
        parents.handles.push_back(pinned.handle); pinned.handle = INVALID_HANDLE_VALUE;
        if (volume) break;
    }
    return true;
}

// Refuse a competing MSI before StopServices can interrupt a real transaction.
// A free lock authorizes no record: cleanup/admission happen only after the old
// reader has stopped. Pre-commit held-lock admission retains the exact
// SYSTEM/package tuple. A protected terminal journal has no remaining install.
inline bool live_guard(MSIHANDLE install, const std::string& code, const std::string& hash,
                       const std::string& remove, const std::string& remove_hash, bool system_caller) {
    failure_code = 5;
    Parents parents;
    if (!pin_ancestors(install, parents) || !privilege(SE_BACKUP_NAME) || !privilege(SE_RESTORE_NAME)) return false;
    if (cleanup::absent(root())) return true;
    Boundary product, maintenance_root;
    failure_code = 4;
    if (!product.pin(root(), true)) { diagnostic(install, L"Recovery refused redirected/inaccessible product root: " + root()); return false; }
    failure_code = 5;
    if (cleanup::absent(maintenance::directory())) return true;
    if (!maintenance_root.pin(maintenance::directory(), true)) {
        diagnostic(install, L"Recovery refused redirected/inaccessible maintenance root: " + maintenance::directory()); return false;
    }
    HANDLE lock = CreateFileW(maintenance::lock_path().c_str(), GENERIC_READ | GENERIC_WRITE, 0, nullptr,
        OPEN_EXISTING, FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (lock != INVALID_HANDLE_VALUE) {
        const bool ok = maintenance::lock_handle(lock);
        if (!ok) diagnostic(install, L"Recovery refused redirected/multiply-linked maintenance lock: " + maintenance::lock_path());
        CloseHandle(lock); return ok;
    }
    const DWORD error = GetLastError();
    if (error == ERROR_FILE_NOT_FOUND) return true;
    if (error != ERROR_SHARING_VIOLATION) {
        diagnostic(install, L"Recovery could not inspect maintenance lock: " + maintenance::lock_path() + L", error=" + std::to_wstring(error)); return false;
    }
    failure_code = 6;
    admission::Object record;
    const bool trusted_store = protected_path(root()) && protected_path(maintenance::directory());
    // A legacy service can repeatedly inherit this lock into an already
    // committed worker which immediately rejects its rewritten plan. Stopping
    // that service ends the restart loop; it does not interrupt a live install.
    // No record or process is discarded here while a lock owner remains alive.
    if (trusted_store && maintenance::read_record(maintenance::directory() + L"\\pending.json", record, true) && terminal_journal(record)) {
        diagnostic(install, L"Completed transaction permits stopping the legacy service before stale-state recovery."); return true;
    }
    const bool ok = system_caller && trusted_store &&
        maintenance::read_record(maintenance::directory() + L"\\msi-authorization.json", record) &&
        admission::matches(record, code, hash, remove, remove_hash);
    diagnostic(install, ok ? L"Live transaction admitted the exact SYSTEM MSI tuple." : L"Live transaction refused a competing MSI before StopServices.");
    return ok;
}

// Cleanup must run after StopServices but before old product removal: legacy
// readers deny delete sharing, and the old nested uninstall rejects stale state.
inline bool prepare(MSIHANDLE install, bool uninstall) {
    failure_code = 5;
    Parents parents;
    if (!pin_ancestors(install, parents)) return false;
    if (!privilege(SE_RESTORE_NAME) || !privilege(SE_BACKUP_NAME)) return false;
    // Pin each boundary before looking up any child, even when its ACL is
    // untrusted. In particular never create a lock through a maintenance junction.
    if (cleanup::absent(root()) && !maintenance::ensure_directory(root(), product_acl)) return false;
    Boundary product, maintenance_root;
    failure_code = 4;
    if (!product.pin(root(), true)) { diagnostic(install, L"Recovery refused redirected product root: " + root()); return false; }
    failure_code = 5;
    if (cleanup::absent(maintenance::directory())) {
        // Seal the existing product boundary before creating a new child.
        Tree seal{false, 0, install};
        if (!seal.visit(root(), false) || !maintenance::ensure_directory(maintenance::directory(), public_acl)) return false;
    }
    if (!maintenance_root.pin(maintenance::directory(), true)) { diagnostic(install, L"Recovery refused redirected maintenance root: " + maintenance::directory()); return false; }
    HANDLE lock = CreateFileW(maintenance::lock_path().c_str(), GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER, 0, nullptr,
        OPEN_ALWAYS, FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, nullptr);
    if (lock == INVALID_HANDLE_VALUE) {
        // The normal authenticated worker admission handles a held lock. Never
        // alter ACLs, records or worker processes while a live owner holds it.
        return GetLastError() == ERROR_SHARING_VIOLATION;
    }
    Tree tree{false, 0, install};
    bool ok = maintenance::lock_handle(lock) && tree.visit(root(), false);
    if (ok) { tree.widened = tree.widened || !protected_handle(lock); ok = set_acl(lock, private_acl); }
    bool alive = false;
    if (ok) ok = workers(false, alive);
    admission::Object journal;
    const bool parsed = !tree.widened && maintenance::read_record(maintenance::directory() + L"\\pending.json", journal, true);
    const bool terminal = parsed && terminal_journal(journal);
    if (ok && disposition(false, tree.widened, uninstall, alive, terminal) == Disposition::refuse) {
        failure_code = 6; diagnostic(install, L"Recovery refused: transaction worker still running without its lock."); ok = false;
    }
    if (ok && alive) ok = workers(true, alive);
    if (ok) ok = reset_tasks(install);
    if (ok) diagnostic(install, L"Recovery restored owner/ACL on entries=" + std::to_wstring(tree.checked) +
        L", widened protection=" + std::to_wstring(tree.widened));
    if (ok) {
        // A free lock and no live worker means these records cannot authorize a
        // live transaction. Widened ACLs invalidate policy/cache as well.
        if (tree.widened) ok = discard_contents(maintenance::directory(), ID_MAINTENANCE_LOCK, protected_path);
        else {
            const auto transactions = maintenance::directory() + L"\\transactions";
            if (!cleanup::absent(transactions)) ok = discard_contents(transactions, L"", protected_path);
            for (const auto name : {L"pending.json", L"msi-authorization.json"}) {
                const auto path = maintenance::directory() + L"\\" + name;
                if (!cleanup::absent(path) && (!protected_path(path) || !DeleteFileW(path.c_str()))) ok = false;
            }
        }
        diagnostic(install, ok ? L"Recovery: free lock; stale records discarded before old product removal." : L"Recovery: cleanup failed.");
    }
    CloseHandle(lock); return ok;
}
}
