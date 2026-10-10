use crate::ToolResult;
use serde_json::Value;

// tracerpt rejects Rust's canonical verbatim paths even when WPR saved the ETL.
// Keep filesystem identity canonical; adapt only the legacy decoder arguments.
pub(super) fn tracerpt_path(path: &std::path::Path) -> ToolResult<String> {
    let value = path.to_str().ok_or("Non-Unicode ETL decoder path")?;
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        return Ok(format!(r"\\{unc}"));
    }
    if let Some(disk) = value.strip_prefix(r"\\?\") {
        if disk.as_bytes().get(1) != Some(&b':') {
            return Err("ETL decoder cannot address this verbatim device path".into());
        }
        return Ok(disk.to_owned());
    }
    Ok(value.to_owned())
}

pub(super) fn validate_wpr_stop(stdout: &str) -> ToolResult<()> {
    for line in stdout.lines() {
        if let Some(dropped) = line.trim().strip_prefix("This trace has dropped ") {
            let count = dropped
                .split_whitespace()
                .next()
                .ok_or("WPR omitted its dropped-event count")?
                .parse::<u64>()?;
            if count != 0 {
                return Err(
                    format!("WPR saved an incomplete trace: {count} dropped events").into(),
                );
            }
        }
    }
    Ok(())
}

// WPR HeapTracingConfig changes this one value. Snapshot it before enabling,
// preserve its registry type, and never replace unrelated IFEO values/subkeys.
pub(super) const SNAPSHOT_IFEO: &str = r"$ErrorActionPreference='Stop'
$relative = 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\' + $env:BEND_PERF_IMAGE_NAME
$key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($relative,$false)
try {
  $saved = @{key_existed=($null -ne $key); flags_present=$false; value_name=$null; kind=$null; value=$null}
  if ($key) {
    $names = @($key.GetValueNames() | Where-Object { $_ -ieq 'TracingFlags' })
    if ($names.Count -gt 0) {
      $saved.flags_present=$true
      $saved.value_name=$names[0]
      $saved.kind=$key.GetValueKind($names[0]).ToString()
      $saved.value=$key.GetValue($names[0],$null,[Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      if ($saved.kind -in @('DWord','QWord')) { $saved.value=$saved.value.ToString([Globalization.CultureInfo]::InvariantCulture) }
    }
  }
  $saved | ConvertTo-Json -Depth 5 -Compress
} finally { if ($key) { $key.Dispose() } }
";

pub(super) const RESTORE_IFEO: &str = r"$ErrorActionPreference='Stop'
$saved = ConvertFrom-Json -InputObject $env:BEND_PERF_IFEO_STATE
$relative = 'SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\' + $env:BEND_PERF_IMAGE_NAME
$key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($relative,$true)
$remove = $false
try {
  if (!$key -and ($saved.key_existed -or $saved.flags_present)) { $key = [Microsoft.Win32.Registry]::LocalMachine.CreateSubKey($relative) }
  if ($key) {
    if ($saved.flags_present) {
      $kind = [Microsoft.Win32.RegistryValueKind][Enum]::Parse([Microsoft.Win32.RegistryValueKind],$saved.kind)
      $value = $null
      switch ($saved.kind) {
        'DWord' { $value=[int]$saved.value }
        'QWord' { $value=[long]$saved.value }
        'Binary' { $value=[byte[]]$saved.value }
        'None' { $value=[byte[]]$saved.value }
        'MultiString' { $value=[string[]]$saved.value }
        'String' { $value=[string]$saved.value }
        'ExpandString' { $value=[string]$saved.value }
        default { throw 'Unsupported prior IFEO registry value type' }
      }
      $key.SetValue($saved.value_name,$value,$kind)
    } else { $key.DeleteValue('TracingFlags',$false) }
    if (!$saved.key_existed -and $key.GetValueNames().Length -eq 0 -and $key.GetSubKeyNames().Length -eq 0) { $remove=$true }
  }
} finally { if ($key) { $key.Dispose() } }
if ($remove) { [Microsoft.Win32.Registry]::LocalMachine.DeleteSubKey($relative,$false) }
";

pub(super) fn validate_snapshot(state: &Value) -> ToolResult<()> {
    if !state["key_existed"].is_boolean() || !state["flags_present"].is_boolean() {
        return Err("IFEO snapshot did not report key/value presence".into());
    }
    if state["flags_present"] == true
        && (state["value_name"]
            .as_str()
            .is_none_or(|name| !name.eq_ignore_ascii_case("TracingFlags"))
            || !matches!(
                state["kind"].as_str(),
                Some(
                    "DWord"
                        | "QWord"
                        | "Binary"
                        | "None"
                        | "MultiString"
                        | "String"
                        | "ExpandString"
                )
            )
            || state["value"].is_null())
    {
        return Err(
            "Prior IFEO TracingFlags cannot be restored exactly; refusing to change it".into(),
        );
    }
    Ok(())
}
