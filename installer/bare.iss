; Bare installer (Inno Setup 6). Built by `cargo xtask release`, which passes:
;   /DAppVersion=0.3.0  /DSourceDir=<repo>\dist\Bare  /DRepoDir=<repo>  /O<repo>\dist
;
; Installs per user (no admin prompt) into %LOCALAPPDATA%\Programs\Bare, which stays writable
; so Bare's own updater can replace files, and registers Bare as a browser so it can be chosen
; in Windows' Default apps.

#ifndef AppVersion
  #error AppVersion must be defined (/DAppVersion=x.y.z)
#endif

; Never change: Windows identifies the installed app by this id. Bare's updater also uses it
; to update the version shown in Settings -> Apps.
#define AppId "B6FF0361-A243-4B32-97EE-94D5CE3B8EF0"

[Setup]
AppId={{{#AppId}}
AppName=Bare
AppVersion={#AppVersion}
AppVerName=Bare {#AppVersion}
AppPublisher=Bare contributors
AppPublisherURL=https://github.com/sai113/Bare
AppSupportURL=https://github.com/sai113/Bare/issues
AppUpdatesURL=https://github.com/sai113/Bare/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName=Bare
VersionInfoDescription=Bare Browser setup
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\Bare
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=BareSetup-{#AppVersion}
SetupIconFile={#RepoDir}\assets\bare.ico
UninstallDisplayIcon={app}\Bare.exe
UninstallDisplayName=Bare
LicenseFile={#RepoDir}\LICENSE
Compression=lzma2/ultra64
SolidCompression=yes
LZMAUseSeparateProcess=yes
WizardStyle=modern
; Close a running Bare (it holds its own files) before installing or uninstalling.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{userprograms}\Bare"; Filename: "{app}\Bare.exe"; Comment: "Bare web browser"
Name: "{userprograms}\Bare (private window)"; Filename: "{app}\Bare.exe"; Parameters: "--private"; Comment: "Bare with nothing saved to disk"
Name: "{userdesktop}\Bare"; Filename: "{app}\Bare.exe"; Tasks: desktopicon

[Registry]
; --- Bare as a browser (Windows "Default apps") ---
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare"; ValueType: string; ValueName: ""; ValueData: "Bare"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\Bare.exe,0"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\Bare.exe"""
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Bare"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "A minimal, private web browser built on Chromium."
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\Bare.exe,0"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\StartMenu"; ValueType: string; ValueName: "StartMenuInternet"; ValueData: "Bare"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\URLAssociations"; ValueType: string; ValueName: "http"; ValueData: "BareURL"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\URLAssociations"; ValueType: string; ValueName: "https"; ValueData: "BareURL"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\FileAssociations"; ValueType: string; ValueName: ".htm"; ValueData: "BareHTML"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\FileAssociations"; ValueType: string; ValueName: ".html"; ValueData: "BareHTML"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\FileAssociations"; ValueType: string; ValueName: ".pdf"; ValueData: "BareHTML"
Root: HKCU; Subkey: "Software\Clients\StartMenuInternet\Bare\Capabilities\FileAssociations"; ValueType: string; ValueName: ".svg"; ValueData: "BareHTML"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "Bare"; ValueData: "Software\Clients\StartMenuInternet\Bare\Capabilities"; Flags: uninsdeletevalue
; How Windows launches Bare for web links...
Root: HKCU; Subkey: "Software\Classes\BareURL"; ValueType: string; ValueName: ""; ValueData: "Bare URL"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\BareURL"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\BareURL\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\Bare.exe,0"
Root: HKCU; Subkey: "Software\Classes\BareURL\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\Bare.exe"" ""%1"""
; ...and for local files.
Root: HKCU; Subkey: "Software\Classes\BareHTML"; ValueType: string; ValueName: ""; ValueData: "Bare document"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\BareHTML\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\Bare.exe,0"
Root: HKCU; Subkey: "Software\Classes\BareHTML\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\Bare.exe"" ""%1"""

[Run]
Filename: "{app}\Bare.exe"; Description: "Start Bare"; Flags: postinstall nowait skipifsilent

[UninstallDelete]
; Leftovers from Bare's updater inside the install folder. Browsing data in
; %LOCALAPPDATA%\Bare is kept, like other browsers do (asked below).
Type: files; Name: "{app}\.bare-write-test"

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Profile: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Profile := ExpandConstant('{localappdata}\Bare');
    if DirExists(Profile) and not UninstallSilent then
      if MsgBox('Also delete your Bare browsing data (cookies, history, settings)?',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(Profile, True, True, True);
  end;
end;
