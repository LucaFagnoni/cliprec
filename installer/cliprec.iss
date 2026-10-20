; Installer ClipRec per Inno Setup 7 (non compilato su questo PC). Prerequisito: `cargo xtask dist` (crea target\dist\ClipRec).
;
; Compilare:  ISCC.exe installer\cliprec.iss        (esce in target\dist\ClipRec-Setup.exe)
; Installazione per utente, senza amministratore: {localappdata}\Programs\ClipRec.

#define AppName "ClipRec"
#define AppVersion "0.1.0"
#define AppExe "cliprec-app.exe"
; Uguale a APP_ID in crates/cliprec-app/src/notify.rs (serve ai toast).
#define AppUserModelID "ClipRec.App"

[Setup]
AppId={{B6E1B0A4-5C1D-4E55-9E43-0C11C1EC0001}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Luca
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
OutputDir=..\target\dist
OutputBaseFilename=ClipRec-Setup
Compression=lzma2/max
SolidCompression=yes
UninstallDisplayIcon={app}\{#AppExe}
; Chiude cliprec-app.exe prima di sostituire i file (aggiornamenti).
CloseApplications=force
RestartApplications=no

; Firma (nessun certificato disponibile). Con un certificato OV (es. Certum Standard Code Signing):
;  1. in Inno Setup: Tools > Configure Sign Tools, aggiungere uno strumento "certum" con
;     $p = "C:\...\signtool.exe" sign /fd sha256 /tr http://time.certum.pl /td sha256 /n "NOME CERTIFICATO" $f
;     (con il certificato su token SimplySign o carta, il PIN lo chiede il driver);
;  2. togliere il punto e virgola alla riga seguente;
;  3. firmare anche gli exe prima di `cargo xtask dist`, o aggiungere sotto [Files] "Flags: signonce".
;SignTool=certum
;SignedUninstaller=yes

[Languages]
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "Avvia ClipRec con Windows"; Flags: unchecked

[Files]
Source: "..\target\dist\ClipRec\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Icons]
; AppUserModelID uguale a quello di autostart.rs: i toast funzionano dal primo avvio.
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; AppUserModelID: "{#AppUserModelID}"

[Registry]
; Stessa chiave e stesso nome usati da `auto-launch` (autostart.rs); l'app la riallinea
; a config.general.start_with_windows. Argomento --tray come in autostart.rs.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ClipRec"; \
  ValueData: """{app}\{#AppExe}"" --tray"; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "--tray"; Description: "Avvia ClipRec"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Chiude l'app prima di rimuovere i file.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM cliprec-settings.exe"; Flags: runhidden; RunOnceId: "KillSettings"

[Code]
// Alla disinstallazione la configurazione in %APPDATA%\ClipRec resta, salvo scelta.
// Le clip in Videos\ClipRec non si toccano mai.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    if (not UninstallSilent) and
       (MsgBox('Eliminare anche configurazione e log di ClipRec (%APPDATA%\ClipRec e %LOCALAPPDATA%\ClipRec)?' + #13#10 +
               'Le clip salvate non vengono toccate.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
    begin
      DelTree(ExpandConstant('{userappdata}\ClipRec'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\ClipRec'), True, True, True);
    end;
end;
