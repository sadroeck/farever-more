#define WINAPI __stdcall
#define CDECL __cdecl
#define NULL ((void *)0)
#define TRUE 1
#define DLL_PROCESS_ATTACH 1

typedef void *LPVOID;
typedef void *HANDLE;
typedef void *HINSTANCE;
typedef void *HMODULE;
typedef unsigned long DWORD;
typedef unsigned short WCHAR;
typedef int BOOL;
typedef void (WINAPI *FARPROC)(void);
typedef DWORD (WINAPI *LPTHREAD_START_ROUTINE)(LPVOID);

__declspec(dllimport) BOOL WINAPI CloseHandle(HANDLE object);
__declspec(dllimport) HANDLE WINAPI CreateThread(
    LPVOID attributes, unsigned __int64 stack_size,
    LPTHREAD_START_ROUTINE start, LPVOID parameter,
    DWORD flags, DWORD *thread_id);
__declspec(dllimport) BOOL WINAPI DisableThreadLibraryCalls(HMODULE module);
__declspec(dllimport) DWORD WINAPI GetModuleFileNameW(
    HMODULE module, WCHAR *filename, DWORD size);
__declspec(dllimport) FARPROC WINAPI GetProcAddress(HMODULE module, const char *name);
__declspec(dllimport) HMODULE WINAPI LoadLibraryW(const WCHAR *filename);

typedef unsigned int (CDECL *fas_host_start_v0_fn)(void);

static DWORD WINAPI fas_bootstrap(LPVOID unused) {
    WCHAR path[32768];
    DWORD length;
    const WCHAR suffix[] = L"farever-addons\\host.dll";
    unsigned __int64 suffix_length = (sizeof(suffix) / sizeof(suffix[0])) - 1;
    unsigned __int64 index;
    HMODULE host;
    fas_host_start_v0_fn start;
    (void)unused;

    length = GetModuleFileNameW(NULL, path, (DWORD)(sizeof(path) / sizeof(path[0])));
    if (length == 0 || length >= (DWORD)(sizeof(path) / sizeof(path[0])))
        return 1;
    for (index = length; index > 0; --index) {
        if (path[index - 1] == L'\\' || path[index - 1] == L'/') {
            length = (DWORD)index;
            break;
        }
    }
    if ((unsigned __int64)length + suffix_length + 1 >= sizeof(path) / sizeof(path[0]))
        return 2;
    for (index = 0; index <= suffix_length; ++index)
        path[length + index] = suffix[index];

    host = LoadLibraryW(path);
    if (host == NULL)
        return 3;
    start = (fas_host_start_v0_fn)(void *)GetProcAddress(host, "fas_host_start_v0");
    if (start == NULL)
        return 4;
    return start();
}

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID reserved) {
    HANDLE thread;
    (void)reserved;
    if (reason != DLL_PROCESS_ATTACH)
        return TRUE;
    DisableThreadLibraryCalls(module);
    thread = CreateThread(NULL, 0, fas_bootstrap, NULL, 0, NULL);
    if (thread != NULL)
        CloseHandle(thread);
    return TRUE;
}
