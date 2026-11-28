; Inno Setup script for Furcadia Native Client
; Produces a single, trim Windows installer (.exe) with Start Menu integration.

#define MyAppName "Furcadia Native Client"
#define MyAppVersion "2.2.6"
#define MyAppPublisher "Bear Built"
#define MyAppURL "https://github.com/morganlowe/native-furcadia"
#define MyAppExeName "furc-gui.exe"

[Setup]
AppId={{8E9A3C4B-6D2F-4B18-9F42-5C3D1E7A9B01}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=FurcadiaNative-Setup-{#MyAppVersion}-x86_64
SetupIconFile=furcadia-native.ico
UninstallDisplayIcon={app}\furcadia-native.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequiredOverridesAllowed=dialog

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\furc-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\furc-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "furcadia-native.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\furcadia-native.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\furcadia-native.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent
