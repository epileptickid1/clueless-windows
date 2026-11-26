# Clueless

A translucent window for Linux and Windows 11 that listens to a Google Meet or Zoom call,
transcribes it with **Deepgram** and lets a signed-in model answer the questions you are asked.

## Install

Install the CLI for the provider you want to use and sign in under the same OS account that
runs Clueless. Each CLI manages its own sign-in. Use a current version of the CLI for your
provider. Antigravity needs `agy acp` and Codex needs `codex app-server` with service-tier support.

| Provider | First sign-in | Connection |
| --- | --- | --- |
| Claude | `claude auth login` | Claude Code `stream-json` |
| Antigravity | `agy acp --login` | Google Antigravity ACP |
| Codex | `codex login` and sign in with ChatGPT | Codex App Server |

Choose the **Provider** and **Model** in the app. **CLI default** starts a conversation with
the provider's default model. Switching providers or returning to **CLI default** starts a new
provider session. For Codex, **Fast mode** requests the Fast service tier and takes effect on
the next response. It is off by default and the choice is saved between launches.

### Linux

```sh
(umask 077; mkdir -p ~/.config/clueless; cat > ~/.config/clueless/deepgram.key)
                      # once: paste your Deepgram API key, press Enter, then Ctrl+D
./install.sh          # builds and installs to ~/.local/bin
clueless
```

Needs GTK 4.12+, PipeWire or PulseAudio (`pactl`, `parec`), poppler, OpenSSL and Rust.

### Windows 11 (x64)

To run a built package, extract `clueless-win11-x64.zip` and open `Clueless\bin\clueless.exe`.
Keep the extracted folders together. Install the Microsoft Visual C++ x64 Redistributable if
Windows reports a missing MSVC runtime. Install and sign in to the provider CLI before sending
a message, then select it in Clueless.

#### Build requirements

- **Rust.** Run `winget install Rustlang.Rustup`, then reopen PowerShell. Use the
  `x86_64-pc-windows-msvc` toolchain.
- **Visual Studio and Windows SDK.** Hopefully you already have Visual Studio. In Visual Studio
  Installer, make sure **Desktop development with C++** and the **Windows SDK** are installed.
  The standalone Visual Studio Build Tools also work.
- **GTK 4.12+.** Use an x64 MSVC build with `pkg-config` or `pkgconf`. 
	```powershell
	-winget install Rustlang.Rustup
	``` 
	```powershell
	-rustup default stable-msvc
	```
	```powershell
	-winget install Git.Git MSYS2.MSYS2 astral-sh.uv
	```
	```powershell
	-uv tool install gvsbuild
	```
	```powershell
	-uv tool update-shell
	```
	 **close terminal after that and open new one**
	```powershell 
	gvsbuild build --vs-ver vs2026 --vs-install-path "C:\Program Files\Microsoft Visual Studio\18\Community" gtk4
	```
	 **20-30 min build**
	```powershell
	[Environment]::SetEnvironmentVariable("PKG_CONFIG_PATH", "C:\gtk-build\gtk\x64\release\lib\pkgconfig", "User") 
	
	[Environment]::SetEnvironmentVariable("Lib", "C:\gtk-build\gtk\x64\release\lib", "User") 
	
	$oldPath = [Environment]::GetEnvironmentVariable("Path", "User") 
	
	[Environment]::SetEnvironmentVariable("Path", "$oldPath;C:\gtk-build\gtk\x64\release\bin", "User")
	```
[GTK for Windows](https://www.gtk.org/docs/installations/windows/) · [gtk-rs Windows setup](https://gtk-rs.org/gtk4-rs/stable/latest/book/installation_windows.html)

```powershell
.\build-windows.ps1 -GtkRoot 'C:\gtk-build\gtk\x64\release' -RunTests
```

After the build, open `Clueless\bin\clueless.exe` in the staging folder. The script prints
the full path on the `Launch:` line and creates `target\clueless-win11-x64.zip` for distribution.
To package PDF tools, pass `-PopplerRoot` with a Windows Poppler directory containing
`bin\pdfinfo.exe` and `bin\pdftotext.exe`. The app also finds them on `PATH`.

For **Listen**, set `DEEPGRAM_API_KEY` or save the key in
`%LOCALAPPDATA%\Clueless\deepgram.key`. Windows captures the default playback device and, with
**Mic** enabled, the default microphone. Select **All system audio**. Audio capture by app
and global hotkeys are available on Linux. Shortcuts within the Clueless window work on Windows.

## Use

1. **Add materials.** Press the paperclip (Ctrl+O) or drop files on the window. Type what the model
   should do with them, for example *"This is my CV and the job description. Answer as me, briefly."*
   Press Enter.
2. **Open the meeting** in your browser.
3. **Choose the source.** *All system audio* is the default. On Linux, to hear only one
   application, press the refresh button while it is playing sound and pick it from the list.
4. Press **● Listen**. The level bar moves when the call is audible.
5. With **Auto-answer** on, a detected question is answered once the other side stops talking.
   **Answer now** (Ctrl+Enter, or Shift) answers on demand. If there is no question yet, it waits for one.
6. **📷 Screen** (Space) takes a screenshot of the primary screen and asks the selected model to solve
   or answer what it shows, with the call as context. If the model
   is still replying, the screenshot is sent with your next message instead.

Space and Shift work only while you are not typing in the message field, so they never get in the way
of your text. Press Escape or click anywhere else in the window to leave the field. Shift has to be pressed and
released on its own; Shift+Enter and other combinations do nothing special.

Windows captures the primary display. Linux uses Spectacle on KDE Plasma or `grim` on other
supported desktops. The Clueless window is in the picture if it covers the captured screen.

### Pauses and questions

Silence alone never triggers anything. After a question is heard, Clueless waits for about
2.5 seconds of silence, or 5 if the sentence sounds unfinished. Each request repeats the recent
transcript, so a question split by pauses is seen whole. If the model finds the question incomplete,
nothing is shown and Clueless keeps listening. If the speaker goes on shortly after an answer was
requested, the whole question is answered again. These timings are constants at the top of
`src/gate.rs`.

| Control | Meaning |
| --- | --- |
| Slider in the title bar | Background opacity |
| On top | Keep above other windows on Windows and KDE Plasma |
| Mic | Also transcribe your own voice as `You` (use headphones, or it hears the call twice) |
| Language | Set it explicitly. *Multilingual* does not cover Ukrainian or Polish |
| Provider / Model | Choose a signed-in CLI and a model. Claude offers `sonnet`, `haiku`, `opus` |
| Fast mode | Request the Fast service tier for Codex |
| Stop / Escape | Interrupt a reply |
| New chat | Forget the conversation, transcript and sent files |

Files: images (PNG, JPEG, GIF, WebP up to 3.7 MB), PDF, DOCX, PPTX, XLSX, ODT/ODP/ODS, and any
UTF-8 text or source file. Codex receives PDFs as extracted text. For the other providers, PDFs over
100 pages or 20 MB are sent as extracted text.

### Global shortcuts on Linux

Wayland does not let apps grab keys. In *System Settings → Keyboard → Shortcuts* add a command
shortcut while Clueless is running:

```sh
gapplication action io.github.clueless answer   # or: screenshot, listen, stop
```

## Privacy and limits

- While listening, call **audio is sent to Deepgram** (and your microphone's, if **Mic** is on).
  Clueless does not write it to disk. Deepgram's data policy and pricing apply.
- The transcript **text**, your messages and your files are sent to the selected provider when a request is made.
- **📷 Screen** sends an image of your screen to the selected provider.
- The provider CLI may store conversation history and attachments. Its usage limits apply.
- This is an ordinary window: it **is visible** if you share your whole screen.
- Transcribing other people may require their consent, and interviews or exams may forbid
  assistance. Check the rules that apply to you.
- Words appear within about a second; an answer starts a few seconds after the question ends.
  Speech recognition makes mistakes; read answers critically.

## Settings

| Environment variable | Purpose |
| --- | --- |
| `DEEPGRAM_API_KEY` | Used instead of the key file |
| `CLUELESS_CLAUDE` | Path to the `claude` executable |
| `CLUELESS_AGY` | Path to the `agy` executable |
| `CLUELESS_CODEX` | Path to the `codex` executable |

## Develop

```sh
cargo test
```
