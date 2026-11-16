; The Windows installer, built with Inno Setup 6 after `cargo build --release`:
;
;   iscc installer\screen-time-manager.iss        (or: powershell -File installer\build.ps1)
;
; It installs for every account on the PC (administrators only) into
; C:\Program Files\ScreenTimeManager, registers the app's scheduled tasks, and is written to
; target\installer. The version is read from the built exe. /DBin=<dir> packages an exe from another
; folder (CI uses the debug build). /DAppVersion=<x.y.z> packages the exe as another version, which
; only the tests use, to have an update to install.
;
; The scheduled tasks, from the task-*.xml templates next to this script:
;   ScreenTimeManager          starts the app at every sign-in, for every account; standard users may
;                              start it (the watchdog relaunches the app through it), but not change
;                              or delete it
;   ScreenTimeManagerWatchdog  every minute, relaunches the app if it was killed and sends a tamper alert
;   ScreenTimeManagerUpdate    on demand, runs `screen-time-manager.exe --update` as SYSTEM; standard
;                              users may start it, but not change or delete it
;   ScreenTimeManagerSetupRecovery
;                              only while setup runs (see task-recovery.xml): if setup is interrupted
;                              with the first two turned off, it turns them back on at the next boot
;                              or 15 minutes later
;
; Setup closes every copy of the app in every session before replacing it, and the app's updater
; runs it silently as SYSTEM (`/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`), so nothing here may wait
; on the user. Afterwards it writes updating.marker, which tells the watchdog in each session to
; bring the app back without a tamper alert - the installing user's session included: setup
; doesn't start the app itself (see [Run]).
;
; Everything setup does as administrator or SYSTEM in a folder a standard account can write to
; (the shared config folder in ProgramData) goes through `screen-time-manager.exe --secure-folders`
; (src/update.rs, `folders`), which never follows a link there; see SecureFolders.

#ifndef Bin
  #define Bin "..\target\release"
#endif
#define AppExe "screen-time-manager.exe"
#define ExePath AddBackslash(SourcePath) + Bin + "\" + AppExe
#if !FileExists(ExePath)
  #error screen-time-manager.exe not found: build it first, or pass /DBin=<folder with the exe>
#endif
#ifndef AppVersion
  #define AppVersion GetFileProductVersion(ExePath)
  #if AppVersion == ""
    ; The numeric version, which build.rs sets to the same: ISCC under Wine can't read the string.
    #define AppVersion GetVersionNumbersString(ExePath)
  #endif
  ; build.rs writes X.Y.Z.0; drop the fourth part so the version matches the vX.Y.Z release tag.
  #if (Len(AppVersion) - Len(StringChange(AppVersion, ".", "")) == 3) && (Copy(AppVersion, Len(AppVersion) - 1) == ".0")
    #define AppVersion Copy(AppVersion, 1, Len(AppVersion) - 2)
  #endif
#endif
#if AppVersion == ""
  #error The exe has no product version
#endif

#define MainTask "ScreenTimeManager"
#define WatchdogTask "ScreenTimeManagerWatchdog"
#define UpdateTask "ScreenTimeManagerUpdate"
#define RecoveryTask "ScreenTimeManagerSetupRecovery"
; src/update.rs, folders::ARG.
#define SecureFoldersArg "--secure-folders"
; Must match the SecurityDescriptor in task-main.xml and task-update.xml; see there.
#define MainTaskSddl "D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRFX;;;BU)"
#define UpdateTaskSddl "D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FRFX;;;BU)"

[Setup]
AppId={{1D3CBE0B-2B68-4A1E-A8F4-96EE93D3D792}
AppName=Screen Time Manager
AppVersion={#AppVersion}
AppPublisher=Screen Time Manager contributors
AppPublisherURL=https://github.com/dhiorns15/screen-time-manager-for-windows
AppSupportURL=https://github.com/dhiorns15/screen-time-manager-for-windows/issues
AppUpdatesURL=https://github.com/dhiorns15/screen-time-manager-for-windows/releases
; 64-bit Program Files, writable only by administrators, so a standard account can't replace the exe
; the scheduled tasks run. The app and its updater expect this folder.
DefaultDirName={autopf}\ScreenTimeManager
DisableDirPage=yes
DisableProgramGroupPage=yes
; Machine-wide and admin only. This also makes the uninstaller require an administrator, so a
; standard account can't remove the app.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 or later (Windows 11 is also 10.0); setup refuses to run on older versions.
MinVersion=10.0
SetupIconFile=..\resources\app.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName=Screen Time Manager
VersionInfoVersion={#AppVersion}
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
; The app is closed by StopApp below, in every session. Restart Manager only reaches the current
; session, and would offer to close or restart the app itself.
CloseApplications=no
RestartApplications=no
; One setup at a time, across sessions: an update starting while someone runs setup by hand fails
; instead of racing it.
SetupMutex=ScreenTimeManagerSetup,Global\ScreenTimeManagerSetup
OutputDir=..\target\installer
OutputBaseFilename=screen-time-manager-v{#AppVersion}-windows-x64-setup

[Files]
Source: "{#Bin}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
; Scheduled task templates, only extracted to register the tasks.
Source: "task-main.xml"; Flags: dontcopy
Source: "task-watchdog.xml"; Flags: dontcopy
Source: "task-update.xml"; Flags: dontcopy
Source: "task-recovery.xml"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\Screen Time Manager"; Filename: "{app}\{#AppExe}"

[UninstallDelete]
; Written after install (see WriteUpdateMarker) and by the app's updater, which keeps its
; downloads and logs in updates.
Type: files; Name: "{app}\updating.marker"
Type: filesandordirs; Name: "{app}\updates"

; No [Run] entry ("Start Screen Time Manager now"): the watchdog starts the app in every signed-in
; session - the installing user's too, which is the child's when a parent approves the UAC prompt
; over their shoulder - within a minute of setup registering its task again (StartWhenAvailable
; catches up on the missed per-minute run, and updating.marker tells it to start the app without a
; tamper alert). It starts it through the ScreenTimeManager task, whose copies leave quietly if
; they find one running. A Finish-page start raced that (the loser said "already running"), and
; unticking it didn't stop the app starting anyway.

[Code]
const
  // ITaskFolder::RegisterTask (taskschd.h).
  TASK_CREATE_OR_UPDATE = 6;
  TASK_LOGON_GROUP = 4;
  TASK_LOGON_SERVICE_ACCOUNT = 5;
  TASK_STATE_RUNNING = 4;

  GENERIC_WRITE = $40000000;
  FILE_SHARE_ALL = 7;
  OPEN_EXISTING = 3;

  // Setup's exit code when the files were installed but the scheduled tasks or the folder
  // permissions could not be set up (see the setup log).
  SetupIncompleteExitCode = 10;

  // `--secure-folders` exit code bits (src/update.rs, folders::run).
  SharedFolderFailed = 1;
  InstallFolderUnsafe = 2;

var
  // The main and watchdog tasks were disabled by PrepareToInstall and not yet registered again;
  // DeinitializeSetup turns them back on if setup stops before that, and the recovery task if
  // setup itself never gets there.
  TasksPaused: Boolean;
  // The recovery task (task-recovery.xml) is registered; removed once the tasks are on again.
  RecoveryRegistered: Boolean;
  SetupIncomplete: Boolean;
  // What went wrong in CurStepChanged, for the message and the log.
  Problems: String;

function CreateFile(FileName: String; DesiredAccess, ShareMode, SecurityAttributes,
  CreationDisposition, FlagsAndAttributes: Cardinal; TemplateFile: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): BOOL;
  external 'CloseHandle@kernel32.dll stdcall';
function GetCurrentProcessId: Cardinal;
  external 'GetCurrentProcessId@kernel32.dll stdcall';
// The PC's awake time in 100 ns units (it stops while asleep, and no clock change moves it).
function QueryUnbiasedInterruptTime(var UnbiasedTime: Int64): BOOL;
  external 'QueryUnbiasedInterruptTime@kernel32.dll stdcall';

function InstalledExe: String;
begin
  Result := ExpandConstant('{app}\{#AppExe}');
end;

// ---- Scheduled tasks, through the Task Scheduler's scripting interface ----

function TaskFolder: Variant;
var
  Service: Variant;
begin
  Service := CreateOleObject('Schedule.Service');
  Service.Connect();
  Result := Service.GetFolder('\');
end;

function FindTask(Folder: Variant; const Name: String; var Task: Variant): Boolean;
begin
  try
    Task := Folder.GetTask(Name);
    Result := True;
  except
    Result := False;
  end;
end;

// Disables a task, so nothing starts it again, and ends its running instances in every session.
// Returns whether it exists.
function PauseTask(Folder: Variant; const Name: String): Boolean;
var
  Task: Variant;
begin
  Result := FindTask(Folder, Name, Task);
  if not Result then
    Exit;
  try
    Task.Enabled := False;
    Task.Stop(0);
    Log('Disabled and ended task ' + Name);
  except
    Log('Could not disable or end task ' + Name + ': ' + GetExceptionMessage);
  end;
end;

// Returns False only if the task exists and could not be enabled.
function EnableTask(Folder: Variant; const Name: String): Boolean;
var
  Task: Variant;
begin
  Result := True;
  if FindTask(Folder, Name, Task) then
  try
    Task.Enabled := True;
    Log('Enabled task ' + Name);
  except
    Result := False;
    Log('Could not enable task ' + Name + ': ' + GetExceptionMessage);
  end;
end;

procedure DeleteTask(Folder: Variant; const Name: String);
begin
  if PauseTask(Folder, Name) then
  try
    Folder.DeleteTask(Name, 0);
    Log('Deleted task ' + Name);
  except
    Log('Could not delete task ' + Name + ': ' + GetExceptionMessage);
  end;
end;

function XmlEscape(const S: String): String;
begin
  Result := S;
  StringChangeEx(Result, '&', '&amp;', True);
  StringChangeEx(Result, '<', '&lt;', True);
  StringChangeEx(Result, '>', '&gt;', True);
end;

// One of the task-*.xml templates with @INSTALL_DIR@ replaced by InstallDir (XML-escaped here). It's
// passed to Task Scheduler as a string, so there is no file encoding for it to get wrong.
function TaskXmlWith(const Template, InstallDir: String): String;
var
  Raw: AnsiString;
begin
  ExtractTemporaryFile(Template);
  if not LoadStringFromFile(ExpandConstant('{tmp}\') + Template, Raw) then
    RaiseException('Could not read ' + Template);
  // The templates are ASCII; the install folder is substituted after conversion, so it may not be.
  Result := String(Raw);
  if StringChangeEx(Result, '@INSTALL_DIR@', XmlEscape(InstallDir), True) = 0 then
    RaiseException(Template + ' has no @INSTALL_DIR@');
end;

// One of the task-*.xml templates, pointing at the install folder.
function TaskXml(const Template: String): String;
begin
  Result := TaskXmlWith(Template, ExpandConstant('{app}'));
end;

// task-recovery.xml: the install folder goes into a PowerShell single-quoted string there.
function RecoveryXml: String;
var
  Quoted: String;
begin
  Quoted := ExpandConstant('{app}');
  StringChangeEx(Quoted, '''', '''''', True);
  Result := TaskXmlWith('task-recovery.xml', Quoted);
end;

// The text of the first <Element> in Xml, or '' if there is none.
function XmlElement(const Xml, Element: String): String;
var
  Start, Len: Integer;
begin
  Result := '';
  Start := Pos('<' + Element + '>', Xml);
  if Start = 0 then
    Exit;
  Start := Start + Length(Element) + 2;
  Len := Pos('</' + Element + '>', Xml) - Start;
  if Len > 0 then
    Result := Copy(Xml, Start, Len);
end;

// Registers (or replaces in place) a task. Replacing also takes over the tasks of the same names
// that install.ps1 registered, whatever folder they ran the exe from. No user or password is
// passed, so the template's principal is used; the logon type passed matches it. Sddl, if not '',
// is the task's security descriptor.
procedure RegisterTaskXml(Folder: Variant; const Name, Xml: String; LogonType: Integer; const Sddl: String);
var
  Task, SddlArg: Variant;
begin
  if Sddl = '' then
    SddlArg := Unassigned
  else
    SddlArg := Sddl;
  Task := Folder.RegisterTask(Name, Xml, TASK_CREATE_OR_UPDATE, Unassigned, Unassigned, LogonType, SddlArg);
  // Set again on the registered task: replacing a task that already exists isn't documented to
  // replace its descriptor, and an upgrade from 1.0.1 must give the main task its new one (standard
  // users couldn't run it, so the watchdog couldn't relaunch the app).
  if Sddl <> '' then
    Task.SetSecurityDescriptor(Sddl, 0);
  // A task replaced in place keeps being disabled otherwise (PauseTask).
  Task.Enabled := True;
  Log('Registered task ' + Name + ' for ' + InstalledExe);
  try
    Log('Task ' + Name + ' security: ' + Task.GetSecurityDescriptor(4));
  except
    Log('Could not read the security of task ' + Name + ': ' + GetExceptionMessage);
  end;
end;

// Whether the update task is running and already as task-update.xml would register it: the same
// revision (its <Version>) and exe.
function UpdateTaskRunningAndCurrent(Folder: Variant; const Xml: String): Boolean;
var
  Task, Definition, Info, Actions, Action: Variant;
  Revision, Path: String;
begin
  Result := False;
  if not FindTask(Folder, '{#UpdateTask}', Task) then
    Exit;
  try
    if Task.State <> TASK_STATE_RUNNING then
      Exit;
    Definition := Task.Definition;
    Info := Definition.RegistrationInfo;
    Revision := Info.Version;
    Actions := Definition.Actions;
    Action := Actions.Item(1);
    Path := Action.Path;
    Result := (Revision = XmlElement(Xml, 'Version')) and (CompareText(RemoveQuotes(Path), InstalledExe) = 0);
  except
    Log('Could not read task {#UpdateTask}: ' + GetExceptionMessage);
  end;
end;

// Registers the app's tasks. WithUpdateTask is False when the install folder could not be made
// safe from standard accounts: the update task runs its exe as SYSTEM, so it's removed instead.
procedure RegisterTasks(WithUpdateTask: Boolean);
var
  Folder, Task: Variant;
  UpdateXml: String;
begin
  Folder := TaskFolder;
  RegisterTaskXml(Folder, '{#MainTask}', TaskXml('task-main.xml'), TASK_LOGON_GROUP, '{#MainTaskSddl}');
  RegisterTaskXml(Folder, '{#WatchdogTask}', TaskXml('task-watchdog.xml'), TASK_LOGON_GROUP, '');
  if not WithUpdateTask then
  begin
    // Turned off, not ended or deleted: this setup may be running inside it (see below). The next
    // setup that finds the folder safe registers it again.
    if FindTask(Folder, '{#UpdateTask}', Task) then
    begin
      Task.Enabled := False;
      Log('Disabled task {#UpdateTask}: the install folder is not safe for SYSTEM to run from');
    end;
    Exit;
  end;
  // Never ended or deleted during install: when the app's updater started this setup, setup may be
  // running inside this task's job, and ending the task would end setup with it. It's replaced in
  // place, and while it's running only if its definition changed.
  UpdateXml := TaskXml('task-update.xml');
  if UpdateTaskRunningAndCurrent(Folder, UpdateXml) then
    Log('Task {#UpdateTask} is running and up to date; left as it is')
  else
    RegisterTaskXml(Folder, '{#UpdateTask}', UpdateXml, TASK_LOGON_SERVICE_ACCOUNT, '{#UpdateTaskSddl}');
end;

// Registers the recovery task (task-recovery.xml). Raises if it can't be.
procedure RegisterRecoveryTask(Folder: Variant);
begin
  RegisterTaskXml(Folder, '{#RecoveryTask}', RecoveryXml, TASK_LOGON_SERVICE_ACCOUNT, '');
  RecoveryRegistered := True;
end;

// Removes the recovery task once the tasks it would turn back on are on. If that fails it's
// harmless: it turns on tasks that are already on, writes the marker and removes itself.
procedure DeleteRecoveryTask;
var
  Folder: Variant;
begin
  if not RecoveryRegistered then
    Exit;
  try
    Folder := TaskFolder;
    Folder.DeleteTask('{#RecoveryTask}', 0);
    RecoveryRegistered := False;
    Log('Deleted task {#RecoveryTask}');
  except
    Log('Could not delete task {#RecoveryTask} (harmless; it removes itself when it runs): ' + GetExceptionMessage);
  end;
end;

// ---- Closing the app ----

procedure Kill(Pid: Integer);
var
  Code: Integer;
begin
  // Elevated (or SYSTEM), taskkill can end the app in any account's session.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /PID ' + IntToStr(Pid), '', SW_HIDE, ewWaitUntilTerminated, Code);
  Log('taskkill /F /PID ' + IntToStr(Pid) + ' exited with ' + IntToStr(Code));
end;

// The processes this setup was started from, as ",pid,pid,...,": the app's updater
// (`screen-time-manager.exe --update`, running as SYSTEM from the install folder) starts setup and
// exits right away, so it's waited for rather than killed.
function AncestorPids(Wmi: Variant): String;
var
  Procs, Proc: Variant;
  Pid, I: Integer;
begin
  Result := ',';
  Pid := GetCurrentProcessId;
  for I := 1 to 6 do
  begin
    Procs := Wmi.ExecQuery('SELECT ParentProcessId FROM Win32_Process WHERE ProcessId = ' + IntToStr(Pid));
    if Procs.Count = 0 then
      Exit;
    Proc := Procs.ItemIndex(0);
    Pid := Proc.ParentProcessId;
    if Pid = 0 then
      Exit;
    Result := Result + IntToStr(Pid) + ',';
  end;
end;

// Whether the installed exe can be overwritten: not while any process still runs it.
function ExeReplaceable: Boolean;
var
  Handle: THandle;
begin
  Result := True;
  if not FileExists(InstalledExe) then
    Exit;
  Handle := CreateFile(InstalledExe, GENERIC_WRITE, FILE_SHARE_ALL, 0, OPEN_EXISTING, 0, 0);
  Result := (Handle <> 0) and (Handle <> THandle(-1));
  if Result then
    CloseHandle(Handle);
end;

// Ends every screen-time-manager.exe, in every session and from any folder (an install.ps1 install
// ran it from wherever it was built), then waits until the installed exe can be replaced. The
// tasks that start it must already be disabled. Returns False if it is still in use after that.
function StopApp: Boolean;
var
  Locator, Wmi, Procs, Proc: Variant;
  Launchers, Waiting: String;
  I, J, Pid, Count: Integer;
begin
  try
    Locator := CreateOleObject('WbemScripting.SWbemLocator');
    Wmi := Locator.ConnectServer('.', 'root\CIMV2');
    Launchers := AncestorPids(Wmi);
    Waiting := '';
    // Up to a minute, giving the process that started setup the first 30 seconds to exit by itself.
    for I := 1 to 120 do
    begin
      Procs := Wmi.ExecQuery('SELECT ProcessId FROM Win32_Process WHERE Name = ''{#AppExe}''');
      Count := Procs.Count;
      if Count = 0 then
        Break;
      for J := 0 to Count - 1 do
      begin
        Proc := Procs.ItemIndex(J);
        Pid := Proc.ProcessId;
        if (I <= 60) and (Pos(',' + IntToStr(Pid) + ',', Launchers) > 0) then
        begin
          if Pos(',' + IntToStr(Pid) + ',', ',' + Waiting) = 0 then
          begin
            Log('Waiting for ' + IntToStr(Pid) + ', which started setup, to exit');
            Waiting := Waiting + IntToStr(Pid) + ',';
          end;
        end else
          Kill(Pid);
      end;
      Sleep(500);
    end;
  except
    // Without WMI, end them by name (the process that started setup included).
    Log('Could not list processes (' + GetExceptionMessage + '); ending ' + '{#AppExe}' + ' by name');
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, I);
  end;

  // An ended process lets go of its exe a moment later.
  for I := 1 to 60 do
  begin
    Result := ExeReplaceable;
    if Result then
      Exit;
    Sleep(500);
  end;
  Log(InstalledExe + ' is still in use');
end;

// ---- Install ----

// The shared config folder (the Telegram/Discord bot settings, the same whichever account runs the
// app), never removed by uninstall, and the install folder, checked as setup leaves it. Done by the
// new exe's `--secure-folders` (src/update.rs, `folders`), run from setup's own temporary folder,
// not from the install folder it checks:
// - the shared folder, %ProgramData%\ScreenTimeManager: whatever a standard account put in its place
//   (ProgramData lets them create folders) - a junction, a link, a file - is removed without being
//   followed, and the folder is created or re-permissioned through a handle opened on the folder
//   itself: owned by Administrators, not inheriting from ProgramData, Users may create files in it
//   and change and delete the files in it, but not delete, rename or re-permission the folder.
//   Until 1.0.2 setup ran `icacls <it> /grant Users:(OI)(CI)M` here, which a junction redirected
//   onto any folder, Program Files included.
// - the install folder: standard accounts must not be able to change anything in it (the update
//   task runs its exe as SYSTEM). Normally it inherits Program Files' permissions and there is
//   nothing to do; anything a standard account could use (an entry granting it write access, a link,
//   a file it created) is removed. Returns False if that couldn't be done.
// Raises if the shared folder couldn't be set up (the app then keeps its bot settings per account).
function SecureFolders: Boolean;
var
  Helper, Params: String;
  Code, I: Integer;
  Output: TExecOutput;
begin
  ExtractTemporaryFile('{#AppExe}');
  Helper := ExpandConstant('{tmp}\{#AppExe}');
  Params := '{#SecureFoldersArg} --shared "' + ExpandConstant('{commonappdata}\ScreenTimeManager')
    + '" --install "' + ExpandConstant('{app}') + '"';
  if not ExecAndCaptureOutput(Helper, Params, ExpandConstant('{tmp}'), SW_HIDE, ewWaitUntilTerminated, Code, Output) then
    RaiseException('Could not run ' + Helper + ': ' + SysErrorMessage(Code));
  for I := 0 to GetArrayLength(Output.StdOut) - 1 do
    Log('Folders: ' + Output.StdOut[I]);
  for I := 0 to GetArrayLength(Output.StdErr) - 1 do
    Log('Folders (stderr): ' + Output.StdErr[I]);
  Log('{#SecureFoldersArg} exited with ' + IntToStr(Code));
  if (Code and not (SharedFolderFailed or InstallFolderUnsafe)) <> 0 then
    RaiseException('Could not check the folders (' + IntToStr(Code) + '); see the setup log');
  Result := (Code and InstallFolderUnsafe) = 0;
  if (Code and SharedFolderFailed) <> 0 then
    RaiseException('Could not set up ' + ExpandConstant('{commonappdata}\ScreenTimeManager') + ' for every account; see the setup log');
end;

// Tells the watchdog in every session that the app was just updated: for 5 minutes after this it
// relaunches the app without a tamper alert. That is how sessions get the app back after setup
// closed it, so setup doesn't start it itself. Written after the files and before the tasks are
// enabled again. After a readable date it records the boot (Windows' BootId) and the PC's awake
// time, by which the watchdog measures those 5 minutes: the wall clock being set right after an
// install (time sync, a parent fixing it) doesn't turn the marker stale and the update into a
// tamper alert. Without them (they couldn't be read) the watchdog goes by the file's time.
procedure WriteUpdateMarker;
var
  Text: String;
  BootId: Cardinal;
  Unbiased: Int64;
begin
  Text := GetDateTimeString('yyyy/mm/dd hh:nn:ss', '-', ':');
  if RegQueryDWordValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters', 'BootId', BootId) then
    if QueryUnbiasedInterruptTime(Unbiased) then
      Text := Text + ' boot=' + IntToStr(BootId) + ' unbiased=' + IntToStr(Unbiased);
  if SaveStringToFile(ExpandConstant('{app}\updating.marker'), Text + #13#10, False) then
    Log('Wrote updating.marker: ' + Text)
  else
    Log('Could not write updating.marker');
end;

// Before any file is replaced: stop anything from starting the app, then close it everywhere. The
// update task is left alone, as it may be running this setup.
//
// The main and watchdog tasks are turned off for this (setting Enabled, which Task Scheduler keeps
// across restarts): a watchdog run or a sign-in would otherwise start the app again in the middle
// of replacing its exe - and until the exe is replaced it's the old version's watchdog, which
// can't be told to wait. So that no interruption can leave them off, the recovery task
// (task-recovery.xml) is registered first: if setup doesn't get to turn them back on itself (in
// CurStepChanged, or DeinitializeSetup when it fails or is cancelled), the recovery task does at
// the next boot, or 15 minutes after this if Windows keeps running. If it can't be registered,
// nothing is turned off and setup stops here.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Folder, Task: Variant;
  HaveTasks: Boolean;
begin
  Result := '';
  HaveTasks := False;
  try
    Folder := TaskFolder;
    HaveTasks := FindTask(Folder, '{#WatchdogTask}', Task) or FindTask(Folder, '{#MainTask}', Task);
  except
    Log('Could not reach the Task Scheduler: ' + GetExceptionMessage);
  end;
  if HaveTasks then
  begin
    try
      RegisterRecoveryTask(Folder);
    except
      Log('Could not register task {#RecoveryTask}: ' + GetExceptionMessage);
      Result := 'Setup could not prepare the Task Scheduler for the update (' + GetExceptionMessage
        + '). Nothing was changed. Restart Windows and run setup again.';
      Exit;
    end;
    // The watchdog first, so it can't relaunch the app in between.
    if PauseTask(Folder, '{#WatchdogTask}') then
      TasksPaused := True;
    if PauseTask(Folder, '{#MainTask}') then
      TasksPaused := True;
  end;
  if not StopApp then
    Result := 'Screen Time Manager is still running and could not be closed. Restart Windows and run setup again.';
end;

procedure AddProblem(const What: String);
begin
  SetupIncomplete := True;
  Log('Setup is incomplete: ' + What);
  if Problems <> '' then
    Problems := Problems + #13#10#13#10;
  Problems := Problems + What;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  InstallFolderSafe: Boolean;
begin
  if CurStep <> ssPostInstall then
    Exit;
  Problems := '';
  // Assume the worst until checked: the update task runs the installed exe as SYSTEM.
  InstallFolderSafe := False;
  try
    InstallFolderSafe := SecureFolders;
    if not InstallFolderSafe then
      AddProblem('Standard accounts may still be able to change files in ' + ExpandConstant('{app}')
        + ', so the update task (which runs as SYSTEM) was turned off.');
  except
    AddProblem(GetExceptionMessage);
  end;
  // The tasks are registered whatever happened above: the app must keep starting.
  try
    WriteUpdateMarker;
    RegisterTasks(InstallFolderSafe);
    TasksPaused := False;
    DeleteRecoveryTask;
  except
    AddProblem('Setting it up to start for every account failed: ' + GetExceptionMessage);
  end;
  if Problems <> '' then
    SuppressibleMsgBox('Screen Time Manager was installed, but not everything could be set up:'
      + #13#10#13#10 + Problems + #13#10#13#10 + 'The setup log has the details. Run setup again to retry.',
      mbCriticalError, MB_OK, IDOK);
end;

// Setup stopped after closing the app (it failed or was cancelled) and before registering the tasks
// again: turn the existing ones back on, so the previous install keeps working. Then the recovery
// task isn't needed any more.
procedure DeinitializeSetup;
var
  Folder: Variant;
begin
  if TasksPaused then
  begin
    Log('Re-enabling the tasks setup disabled');
    try
      if FileExists(InstalledExe) then
        WriteUpdateMarker;
      Folder := TaskFolder;
      // Both tried, whatever the first one does.
      if EnableTask(Folder, '{#MainTask}') then
        TasksPaused := not EnableTask(Folder, '{#WatchdogTask}')
      else
        EnableTask(Folder, '{#WatchdogTask}');
    except
      Log('Could not re-enable the tasks: ' + GetExceptionMessage);
    end;
  end;
  // Left in place if the tasks couldn't be turned back on: then it's the one that does.
  if not TasksPaused then
    DeleteRecoveryTask;
end;

function GetCustomSetupExitCode: Integer;
begin
  if SetupIncomplete then
    Result := SetupIncompleteExitCode
  else
    Result := 0;
end;

// ---- Uninstall ----

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Folder: Variant;
  Kept: String;
begin
  case CurUninstallStep of
    usUninstall:
      begin
        // The tasks first, so nothing relaunches the app while it's closed and removed.
        try
          Folder := TaskFolder;
          DeleteTask(Folder, '{#WatchdogTask}');
          DeleteTask(Folder, '{#MainTask}');
          DeleteTask(Folder, '{#UpdateTask}');
          // Only there if an earlier setup was interrupted.
          DeleteTask(Folder, '{#RecoveryTask}');
        except
          Log('Could not reach the Task Scheduler: ' + GetExceptionMessage);
        end;
        StopApp;
      end;
    usPostUninstall:
      begin
        Kept := 'Settings and history were kept, in case Screen Time Manager is installed again. To remove them too, delete:'
          + #13#10#13#10 + ExpandConstant('{commonappdata}\ScreenTimeManager') + '   (shared Telegram/Discord bot settings)'
          + #13#10 + '%LOCALAPPDATA%\.screen-time-manager in each Windows account   (limits, passcode, history)';
        Log(Kept);
        if not UninstallSilent then
          MsgBox(Kept, mbInformation, MB_OK);
      end;
  end;
end;
