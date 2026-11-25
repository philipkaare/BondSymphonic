; The Windows installer: a per-user install of the folder scripts\package.ps1
; stages (dist\BondSymphonic), with Start menu entries and an uninstaller.
;
;   iscc /DAppVersion=0.1.14 /DSourceDir=..\..\dist\BondSymphonic /DOutputDir=..\..\dist installer\windows\BondSymphonic.iss
;
; Per-user (%LOCALAPPDATA%\Programs\BondSymphonic) and without elevation, so
; the IDE can update itself: the updater runs the next release's installer
; with /VERYSILENT after the IDE exits, into this same folder, and nothing
; asks for administrator rights along the way. The Linux side -- the WSL
; distro -- is not set up here; the IDE does that itself on first start.
;
; /relaunch=1 on the command line starts the IDE again after a silent
; install. The updater passes it for "Restart to update" and leaves it off
; for an update installed on quit.

#ifndef AppVersion
  #error Pass the version: /DAppVersion=x.y.z
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\BondSymphonic"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

[Setup]
; Never change the AppId: it is how an update finds the install it replaces.
AppId={{939F1D1A-22B9-4DF0-821A-D17223AA781F}
AppName=BondSymphonic
AppVersion={#AppVersion}
AppVerName=BondSymphonic {#AppVersion}
AppPublisher=BondSymphonic
AppPublisherURL=https://github.com/philipkaare/BondSymphonic
AppSupportURL=https://github.com/philipkaare/BondSymphonic/issues
AppUpdatesURL=https://github.com/philipkaare/BondSymphonic/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\BondSymphonic
DefaultGroupName=BondSymphonic
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
OutputDir={#OutputDir}
OutputBaseFilename=BondSymphonic-{#AppVersion}-windows-x64-setup
SetupIconFile=..\..\assets\logo.ico
UninstallDisplayIcon={app}\bondsymphonic-ide.exe
UninstallDisplayName=BondSymphonic
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; The IDE has normally exited before an update starts (the updater waits for
; it), but one started by hand from Explorer may find it running: close it
; rather than fail half way through replacing its DLLs.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; install.ps1 is the zip's provisioning script; the installed IDE provisions
; the distro itself, from setup-wsl.ps1 and setup-wsl.sh beside it.
Source: "{#SourceDir}\*"; DestDir: "{app}"; Excludes: "install.ps1"; Flags: ignoreversion recursesubdirs createallsubdirs

[InstallDelete]
; Qt plugins from an older release that the new one no longer ships would
; otherwise stay and be loaded. The folders are rebuilt from [Files].
Type: filesandordirs; Name: "{app}\platforms"
Type: filesandordirs; Name: "{app}\styles"
Type: filesandordirs; Name: "{app}\imageformats"
Type: filesandordirs; Name: "{app}\iconengines"
Type: filesandordirs; Name: "{app}\generic"
Type: filesandordirs; Name: "{app}\networkinformation"
Type: filesandordirs; Name: "{app}\tls"

[Icons]
Name: "{autoprograms}\BondSymphonic"; Filename: "{app}\bondsymphonic-ide.exe"
Name: "{autodesktop}\BondSymphonic"; Filename: "{app}\bondsymphonic-ide.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\bondsymphonic-ide.exe"; Description: "{cm:LaunchProgram,BondSymphonic}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\bondsymphonic-ide.exe"; Flags: nowait; Check: RelaunchRequested

[Code]
function RelaunchRequested: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:relaunch|0}') = '1');
end;
