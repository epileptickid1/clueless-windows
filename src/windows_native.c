/* Win32 entry points that keep SDK types and COM vtables on the C side. */
#define COBJMACROS
#define WIN32_LEAN_AND_MEAN
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0A00
#endif
#include <windows.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <stdint.h>
#include <stdlib.h>
#include <wchar.h>

/* The C SDK headers declare these GUIDs. The C++ declarations carry their values. */
static const CLSID device_enumerator_class = {
    0xbcde0395, 0xe52f, 0x467c, {0x8e, 0x3d, 0xc4, 0x57, 0x92, 0x91, 0x69, 0x2e}
};
static const IID device_enumerator_interface = {
    0xa95664d2, 0x9614, 0x4f35, {0xa7, 0x46, 0xde, 0x8d, 0xb6, 0x36, 0x17, 0xe6}
};
static const IID audio_client_interface = {
    0x1cb9ad4c, 0xdbfa, 0x4c32, {0xb1, 0x78, 0xc2, 0xf5, 0x68, 0xa7, 0x03, 0xb2}
};
static const IID audio_capture_client_interface = {
    0xc8adbd64, 0xe71e, 0x48a0, {0xa4, 0xde, 0x18, 0x5c, 0x39, 0x5c, 0xd3, 0x17}
};

typedef struct {
    IAudioClient *client;
    IAudioCaptureClient *reader;
    HANDLE event;
} CluelessCapture;

static HRESULT last_error(void) {
    DWORD error = GetLastError();
    return error == ERROR_SUCCESS ? E_FAIL : HRESULT_FROM_WIN32(error);
}

static HRESULT default_endpoint(EDataFlow flow, IMMDevice **device) {
    IMMDeviceEnumerator *enumerator = NULL;
    HRESULT result = CoCreateInstance(
        &device_enumerator_class, NULL, CLSCTX_INPROC_SERVER,
        &device_enumerator_interface, (void **)&enumerator);
    if (SUCCEEDED(result)) {
        result = IMMDeviceEnumerator_GetDefaultAudioEndpoint(enumerator, flow, eConsole, device);
        IMMDeviceEnumerator_Release(enumerator);
    }
    return result;
}

HRESULT clueless_default_output(WCHAR **id, size_t *length) {
    HRESULT initialized = CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(initialized) && initialized != RPC_E_CHANGED_MODE) {
        return initialized;
    }
    IMMDevice *device = NULL;
    HRESULT result = default_endpoint(eRender, &device);
    if (SUCCEEDED(result)) {
        result = IMMDevice_GetId(device, id);
        IMMDevice_Release(device);
        if (SUCCEEDED(result)) {
            *length = wcslen(*id);
        }
    }
    if (SUCCEEDED(initialized)) {
        CoUninitialize();
    }
    return result;
}

void clueless_free_device_id(WCHAR *id) {
    CoTaskMemFree(id);
}

static void clear_capture(CluelessCapture *capture) {
    if (capture->reader != NULL) {
        IAudioCaptureClient_Release(capture->reader);
    }
    if (capture->client != NULL) {
        IAudioClient_Release(capture->client);
    }
    if (capture->event != NULL) {
        CloseHandle(capture->event);
    }
    free(capture);
}

HRESULT clueless_capture_open(BOOL mic, DWORD sample_rate, CluelessCapture **out) {
    HRESULT result = CoInitializeEx(NULL, COINIT_MULTITHREADED);
    if (FAILED(result)) {
        return result;
    }
    CluelessCapture *capture = calloc(1, sizeof(*capture));
    if (capture == NULL) {
        CoUninitialize();
        return E_OUTOFMEMORY;
    }

    IMMDevice *device = NULL;
    result = default_endpoint(mic ? eCapture : eRender, &device);
    if (FAILED(result)) {
        goto failed;
    }
    result = IMMDevice_Activate(device, &audio_client_interface, CLSCTX_INPROC_SERVER,
                               NULL, (void **)&capture->client);
    IMMDevice_Release(device);
    if (FAILED(result)) {
        goto failed;
    }

    WAVEFORMATEX format = {0};
    format.wFormatTag = WAVE_FORMAT_PCM;
    format.nChannels = 1;
    format.nSamplesPerSec = sample_rate;
    format.wBitsPerSample = 16;
    format.nBlockAlign = 2;
    format.nAvgBytesPerSec = sample_rate * format.nBlockAlign;
    DWORD flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                  | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if (!mic) {
        flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    /* Request the same PCM stream as parec. WASAPI converts the device mix format. */
    result = IAudioClient_Initialize(capture->client, AUDCLNT_SHAREMODE_SHARED,
                                     flags, 0, 0, &format, NULL);
    if (FAILED(result)) {
        goto failed;
    }
    capture->event = CreateEventW(NULL, FALSE, FALSE, NULL);
    if (capture->event == NULL) {
        result = last_error();
        goto failed;
    }
    result = IAudioClient_SetEventHandle(capture->client, capture->event);
    if (FAILED(result)) {
        goto failed;
    }
    result = IAudioClient_GetService(capture->client, &audio_capture_client_interface,
                                     (void **)&capture->reader);
    if (FAILED(result)) {
        goto failed;
    }
    result = IAudioClient_Start(capture->client);
    if (SUCCEEDED(result)) {
        *out = capture;
        return S_OK;
    }

failed:
    clear_capture(capture);
    CoUninitialize();
    return result;
}

HRESULT clueless_capture_wait(CluelessCapture *capture, DWORD milliseconds) {
    DWORD result = WaitForSingleObject(capture->event, milliseconds);
    if (result == WAIT_OBJECT_0) {
        return S_OK;
    }
    if (result == WAIT_TIMEOUT) {
        return S_FALSE;
    }
    return last_error();
}

HRESULT clueless_capture_read(CluelessCapture *capture, BYTE **data, UINT32 *frames, BOOL *silent) {
    UINT32 available = 0;
    HRESULT result = IAudioCaptureClient_GetNextPacketSize(capture->reader, &available);
    if (FAILED(result)) {
        return result;
    }
    if (available == 0) {
        return S_FALSE;
    }
    DWORD flags = 0;
    result = IAudioCaptureClient_GetBuffer(capture->reader, data, frames, &flags, NULL, NULL);
    if (SUCCEEDED(result)) {
        *silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0;
        return *frames == 0 ? S_FALSE : S_OK;
    }
    return result;
}

HRESULT clueless_capture_release(CluelessCapture *capture, UINT32 frames) {
    return IAudioCaptureClient_ReleaseBuffer(capture->reader, frames);
}

void clueless_capture_close(CluelessCapture *capture) {
    IAudioClient_Stop(capture->client);
    clear_capture(capture);
    CoUninitialize();
}

typedef struct {
    BYTE *pixels;
    int width;
    int height;
} CluelessScreen;

HRESULT clueless_screen_capture(CluelessScreen *out) {
    /* Obtain physical pixels even when the main GTK thread uses another DPI context. */
    DPI_AWARENESS_CONTEXT old_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    if (old_dpi == NULL) {
        return last_error();
    }
    HDC display = NULL;
    HDC memory = NULL;
    HBITMAP bitmap = NULL;
    HGDIOBJ previous = NULL;
    HRESULT result = E_FAIL;
    POINT origin = {0, 0};
    HMONITOR monitor = MonitorFromPoint(origin, MONITOR_DEFAULTTOPRIMARY);
    MONITORINFO monitor_info = {0};
    monitor_info.cbSize = sizeof(monitor_info);
    if (!GetMonitorInfoW(monitor, &monitor_info)) {
        result = last_error();
        goto done;
    }
    RECT bounds = monitor_info.rcMonitor;
    int width = bounds.right - bounds.left;
    int height = bounds.bottom - bounds.top;
    if (width <= 0 || height <= 0 || (size_t)width > SIZE_MAX / 4 / (size_t)height) {
        goto done;
    }
    display = GetDC(NULL);
    if (display == NULL) {
        result = last_error();
        goto done;
    }
    memory = CreateCompatibleDC(display);
    if (memory == NULL) {
        result = last_error();
        goto done;
    }
    BITMAPINFO info = {0};
    info.bmiHeader.biSize = sizeof(info.bmiHeader);
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    void *bits = NULL;
    bitmap = CreateDIBSection(display, &info, DIB_RGB_COLORS, &bits, NULL, 0);
    if (bitmap == NULL || bits == NULL) {
        result = last_error();
        goto done;
    }
    previous = SelectObject(memory, bitmap);
    if (previous == NULL || previous == HGDI_ERROR) {
        result = last_error();
        goto done;
    }
    if (!BitBlt(memory, 0, 0, width, height, display, bounds.left, bounds.top, SRCCOPY | CAPTUREBLT)
        || !GdiFlush()) {
        result = last_error();
        goto done;
    }

    size_t count = (size_t)width * (size_t)height;
    BYTE *pixels = malloc(count * 3);
    if (pixels == NULL) {
        result = E_OUTOFMEMORY;
        goto done;
    }
    const BYTE *bgrx = bits;
    for (size_t i = 0; i < count; i++) {
        pixels[i * 3] = bgrx[i * 4 + 2];
        pixels[i * 3 + 1] = bgrx[i * 4 + 1];
        pixels[i * 3 + 2] = bgrx[i * 4];
    }
    out->pixels = pixels;
    out->width = width;
    out->height = height;
    result = S_OK;

done:
    if (previous != NULL && previous != HGDI_ERROR) {
        SelectObject(memory, previous);
    }
    if (bitmap != NULL) {
        DeleteObject(bitmap);
    }
    if (memory != NULL) {
        DeleteDC(memory);
    }
    if (display != NULL) {
        ReleaseDC(NULL, display);
    }
    SetThreadDpiAwarenessContext(old_dpi);
    return result;
}

void clueless_screen_free(BYTE *pixels) {
    free(pixels);
}

HRESULT clueless_set_topmost(HWND hwnd, BOOL above) {
    return SetWindowPos(hwnd, above ? HWND_TOPMOST : HWND_NOTOPMOST,
                        0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)
               ? S_OK
               : last_error();
}
