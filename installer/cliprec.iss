; Installer ClipRec per Inno Setup 7 (non compilato su questo PC). Prerequisito: `cargo xtask dist` (crea target\dist\ClipRec).
;
; Compilare:  ISCC.exe installer\cliprec.iss        (esce in target\dist\ClipRec-Setup.exe)
; Installazione per utente, senza amministratore: {localappdata}\Programs\ClipRec.

#define AppName "ClipRec"
; Da `ISCC /DAppVersion=x.y.z`; altrimenti questa.
#ifndef AppVersion
  #define AppVersion "0.1.10"
#endif
#define AppExe "cliprec-app.exe"
; Uguale a APP_ID in crates/cliprec-app/src/notify.rs (serve ai toast).
#define AppUserModelID "ClipRec.App"

[Setup]
AppId={{B6E1B0A4-5C1D-4E55-9E43-0C11C1EC0001}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=ClipRec
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
OutputDir=..\target\dist
OutputBaseFilename=ClipRec-{#AppVersion}-Setup
Compression=lzma2/max
SolidCompression=yes
UninstallDisplayIcon={app}\{#AppExe}
SetupIconFile=..\assets\icon\cliprec.ico
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

[CustomMessages]
italian.TaskAutostart=Avvia ClipRec con Windows
english.TaskAutostart=Start ClipRec with Windows
italian.RunApp=Avvia ClipRec
english.RunApp=Start ClipRec
italian.VCRedistStatus=Installazione del runtime Visual C++ (richiede l'autorizzazione di Windows)...
english.VCRedistStatus=Installing the Visual C++ runtime (Windows will ask for permission)...
italian.VCRedistFailed=Impossibile scaricare il runtime Visual C++ (%1).%nClipRec funziona lo stesso; potrai installarlo più tardi da %2
english.VCRedistFailed=Could not download the Visual C++ runtime (%1).%nClipRec works anyway; you can install it later from %2
italian.DeleteData=Eliminare anche configurazione e log di ClipRec (%APPDATA%\ClipRec e %LOCALAPPDATA%\ClipRec)?%nLe clip salvate non vengono toccate.
english.DeleteData=Also delete ClipRec's settings and logs (%APPDATA%\ClipRec and %LOCALAPPDATA%\ClipRec)?%nSaved clips are not touched.

[Tasks]
Name: "autostart"; Description: "{cm:TaskAutostart}"; Flags: unchecked

[Files]
Source: "..\target\dist\ClipRec\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs
; Visual C++ Redistributable (scelta di Luca): non è nel pacchetto. Se manca sul PC (VCRedistNeeded)
; l'installer lo scarica da Microsoft prima di installare (NextButtonClick) e lo lancia in [Run].
; ATTENZIONE: il redistributable chiede l'elevazione (UAC) anche se l'installer di ClipRec è per
; utente e senza amministratore. Senza rete ClipRec parte lo stesso: la cartella contiene le DLL
; del runtime accanto agli exe (distribuzione app-local).

[Icons]
; AppUserModelID uguale a quello di autostart.rs: i toast funzionano dal primo avvio.
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; AppUserModelID: "{#AppUserModelID}"

[Registry]
; Stessa chiave e stesso nome usati da autostart.rs; l'app la riallinea
; a config.general.start_with_windows. Argomento --tray come in autostart.rs.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "ClipRec"; \
  ValueData: """{app}\{#AppExe}"" --tray"; Flags: uninsdeletevalue; Tasks: autostart
; Lingua scelta qui: ClipRec la legge al primo avvio, la scrive in config.toml e cancella il valore.
Root: HKCU; Subkey: "Software\ClipRec"; ValueType: string; ValueName: "InstallerLanguage"; ValueData: "{language}"; Flags: uninsdeletekey

[Run]
Filename: "{tmp}\vc_redist.x64.exe"; Parameters: "/install /quiet /norestart"; StatusMsg: "{cm:VCRedistStatus}"; Check: VCRedistDownloaded
Filename: "{app}\{#AppExe}"; Parameters: "--tray"; Description: "{cm:RunApp}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Chiude l'app prima di rimuovere i file.
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM cliprec-settings.exe"; Flags: runhidden; RunOnceId: "KillSettings"

[Code]
const
  VCRedistUrl = 'https://aka.ms/vs/17/release/vc_redist.x64.exe';

var
  DownloadPage: TDownloadWizardPage;
  RedistReady: Boolean;

// Runtime VC++ 14.x x64 già presente? (valore Installed=1, vista a 64 bit e WOW6432Node)
function VCRedistNeeded: Boolean;
var
  v: Cardinal;
begin
  Result := not (
    (RegQueryDWordValue(HKLM64, 'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64', 'Installed', v) and (v = 1)) or
    (RegQueryDWordValue(HKLM64, 'SOFTWARE\WOW6432Node\Microsoft\VisualStudio\14.0\VC\Runtimes\x64', 'Installed', v) and (v = 1)));
end;

// Scaricato in questa installazione: [Run] lo lancia solo in questo caso.
function VCRedistDownloaded: Boolean;
begin
  Result := RedistReady;
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing), SetupMessage(msgPreparingDesc), nil);
end;

// Su «Installa»: se il runtime manca lo scarica da Microsoft (circa 25 MB) con la pagina di
// avanzamento. Se il download fallisce o viene annullato si avvisa e si prosegue: ClipRec usa le
// DLL del runtime nella sua cartella.
function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID <> wpReady) or (not VCRedistNeeded) then
    Exit;
  DownloadPage.Clear;
  DownloadPage.Add(VCRedistUrl, 'vc_redist.x64.exe', '');
  DownloadPage.Show;
  try
    try
      DownloadPage.Download;
      RedistReady := True;
    except
      if not DownloadPage.AbortedByUser then
        SuppressibleMsgBox(FmtMessage(CustomMessage('VCRedistFailed'), [GetExceptionMessage, VCRedistUrl]), mbInformation, MB_OK, IDOK);
    end;
  finally
    DownloadPage.Hide;
  end;
end;

// Alla disinstallazione la configurazione in %APPDATA%\ClipRec resta, salvo scelta.
// Le clip in Videos\ClipRec non si toccano mai.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    if (not UninstallSilent) and
       (MsgBox(CustomMessage('DeleteData'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
    begin
      DelTree(ExpandConstant('{userappdata}\ClipRec'), True, True, True);
      DelTree(ExpandConstant('{localappdata}\ClipRec'), True, True, True);
    end;
end;
