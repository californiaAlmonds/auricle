; Inno Setup script for Auricle. Built in CI via ISCC (preinstalled on the
; windows-latest runner). Produces auricle-<ver>-setup.exe with proper silent
; switches (/VERYSILENT) so winget/Chocolatey can install unattended.
;
; CI passes the version as: ISCC /DMyAppVersion=0.1.2 packaging\inno\auricle.iss
; and expects the no-self-update binary at dist\auricle.exe.

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif

[Setup]
AppId={{8A2D6F4E-1C3B-4E5A-9B7C-AURICLE000001}
AppName=Auricle
AppVersion={#MyAppVersion}
AppPublisher=californiaAlmonds
AppPublisherURL=https://github.com/californiaAlmonds/auricle
DefaultDirName={autopf}\Auricle
DefaultGroupName=Auricle
DisableProgramGroupPage=yes
OutputDir=dist
OutputBaseFilename=auricle-{#MyAppVersion}-setup
SetupIconFile=icons\icon.ico
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequiredOverridesAllowed=dialog
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Files]
Source: "dist\auricle.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Auricle"; Filename: "{app}\auricle.exe"
Name: "{autodesktop}\Auricle"; Filename: "{app}\auricle.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\auricle.exe"; Description: "Launch Auricle"; Flags: nowait postinstall skipifsilent
