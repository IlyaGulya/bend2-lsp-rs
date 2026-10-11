use crate::ToolResult;
use serde_json::Value;

// Keep filesystem identity canonical; use ordinary Win32 paths for tracing tools.
pub(super) fn decoder_path(path: &std::path::Path) -> ToolResult<String> {
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
        if let Some(dropped) = line.trim().strip_prefix("This trace has dropped") {
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

// Export the installed native profile rather than copying its provider/stack
// definitions. Fixed file-mode buffers absorb bursts without circular overwrite;
// no keywords, samples, allocation events, or stacks are removed.
// Microsoft WPRControlProfiles schema: EventBufferElementGroup ordering and
// Buffers/BufferSize values; -exportprofile preserves native profile semantics.
// Native WPR's CProfileElement parser rejects StackCaching in *CollectorId
// overrides with E_UNEXPECTED (0x8000ffff), despite the published schema.
// Configure the cache once on each collector definition, not its references.
pub(super) const CONFIGURE_WPR: &str = r"$ErrorActionPreference='Stop'
$profile = New-Object System.Xml.XmlDocument
$profile.PreserveWhitespace = $true
$profile.Load($env:BEND_PERF_WPR_SOURCE)
if ($profile.DocumentElement.Name -ne 'WindowsPerformanceRecorder') { throw 'Unexpected native WPR profile root' }
$selected = @($profile.SelectNodes('/WindowsPerformanceRecorder/Profiles/Profile') | Where-Object { $_.Name -eq $env:BEND_PERF_WPR_PROFILE_NAME -and $_.LoggingMode -eq 'File' })
if ($selected.Count -eq 0) { throw 'Native WPR export omitted the requested file-mode profile' }
$collectors = @($profile.SelectNodes('//SystemCollector | //EventCollector | //HeapEventCollector | //SystemCollectorId | //EventCollectorId | //HeapEventCollectorId'))
if ($collectors.Count -eq 0) { throw 'Native WPR export omitted its collectors' }
if ($env:BEND_PERF_WPR_PROFILE_NAME -eq 'Heap' -and !$profile.SelectSingleNode('//HeapEventCollector')) { throw 'Native Heap export omitted its heap collector' }
foreach ($collector in $collectors) {
  $count = switch ($collector.LocalName) {
    { $_ -in @('HeapEventCollector','HeapEventCollectorId') } { 512; break }
    { $_ -in @('SystemCollector','SystemCollectorId') } { 256; break }
    default { 64 }
  }
  $size = $collector.SelectSingleNode('BufferSize')
  if (!$size) { $size=$profile.CreateElement('BufferSize'); [void]$collector.PrependChild($size) }
  $size.SetAttribute('Value','1024')
  $buffers = $collector.SelectSingleNode('Buffers')
  if (!$buffers) { $buffers=$profile.CreateElement('Buffers'); [void]$collector.InsertAfter($buffers,$size) }
  $buffers.SetAttribute('Value',$count.ToString([Globalization.CultureInfo]::InvariantCulture))
  $buffers.RemoveAttribute('PercentageOfTotalMemory')
  $buffers.RemoveAttribute('MaximumBufferSpace')
  $buffers.RemoveAttribute('MinimumRundownSpace')
  if ($collector.LocalName -in @('SystemCollectorId','EventCollectorId','HeapEventCollectorId')) { continue }
  $cache = $collector.SelectSingleNode('StackCaching')
  if (!$cache) { $cache=$profile.CreateElement('StackCaching'); [void]$collector.InsertAfter($cache,$buffers) }
  if ($collector.LocalName -eq 'HeapEventCollector') {
    $cache.SetAttribute('BucketCount','4096')
    $cache.SetAttribute('CacheSize','65536')
  } else {
    $cache.SetAttribute('BucketCount','1024')
    $cache.SetAttribute('CacheSize','16384')
  }
}
$profile.Save($env:BEND_PERF_WPR_PROFILE)
";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_wpr_loss_reports_remain_fatal() {
        // Hosted x64 run 38082284299: WPR exited successfully and saved ETLs,
        // but both stop reports explicitly declared incomplete recordings.
        for count in [50_716, 84_437_624] {
            let report = format!(
                "\r\nThe trace has been successfully saved.\r\n\r\nThis trace has dropped {count} events. Please record this trace again.\r\n"
            );
            let result = validate_wpr_stop(&report);
            assert!(result.is_err(), "Accepted a saved but incomplete ETL");
            if let Err(error) = result {
                assert!(error.to_string().contains(&count.to_string()), "{error}");
            }
        }
        assert!(validate_wpr_stop("This trace has dropped invalid events.\r\n").is_err());
        assert!(validate_wpr_stop("This trace has dropped \r\n").is_err());
    }

    #[test]
    fn complete_wpr_stop_without_losses_is_accepted() -> ToolResult<()> {
        validate_wpr_stop("\r\nThe trace has been successfully saved.\r\n")?;
        validate_wpr_stop("This trace has dropped 0 events.\r\n")
    }
    #[cfg(windows)]
    #[test]
    fn bounded_profiles_preserve_the_installed_native_cpu_and_heap_schema() -> ToolResult<()> {
        // Export/configure/decode profiles only: this test never starts a collector.
        for name in ["CPU", "Heap"] {
            let directory = tempfile::tempdir()?;
            let source = directory.path().join("source.wprp");
            let profile = directory.path().join("recording.wprp");
            let exported = std::process::Command::new("wpr")
                .args(["-exportprofile", name])
                .arg(&source)
                .arg("-filemode")
                .output()?;
            assert!(exported.status.success(), "{exported:?}");
            let original_details = std::process::Command::new("wpr")
                .arg("-profiledetails")
                .arg(format!("{}!{name}", source.display()))
                .arg("-filemode")
                .output()?;
            assert!(original_details.status.success(), "{original_details:?}");
            let configured = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", CONFIGURE_WPR])
                .env("BEND_PERF_WPR_SOURCE", &source)
                .env("BEND_PERF_WPR_PROFILE", &profile)
                .env("BEND_PERF_WPR_PROFILE_NAME", name)
                .output()?;
            assert!(configured.status.success(), "{configured:?}");
            const CHECK: &str = r"$ErrorActionPreference='Stop'
$before = New-Object System.Xml.XmlDocument
$after = New-Object System.Xml.XmlDocument
$before.Load($env:BEND_PERF_WPR_SOURCE)
$after.Load($env:BEND_PERF_WPR_PROFILE)
foreach ($collector in $after.SelectNodes('//SystemCollector | //EventCollector | //HeapEventCollector | //SystemCollectorId | //EventCollectorId | //HeapEventCollectorId')) {
  if ($collector.BufferSize.Value -ne '1024') { throw 'Collector size not configured' }
  $expected = switch ($collector.LocalName) {
    { $_ -in @('HeapEventCollector','HeapEventCollectorId') } { '512'; break }
    { $_ -in @('SystemCollector','SystemCollectorId') } { '256'; break }
    default { '64' }
  }
  if ($collector.Buffers.Value -ne $expected -or $collector.Buffers.HasAttribute('PercentageOfTotalMemory')) { throw 'Collector buffer count is not bounded' }
  if ($collector.LocalName -in @('SystemCollectorId','EventCollectorId','HeapEventCollectorId')) {
    if ($collector.StackCaching) { throw 'Stack caching on a collector reference is not supported by native WPR' }
  } else {
    $buckets = if ($collector.LocalName -eq 'HeapEventCollector') { '4096' } else { '1024' }
    $cacheSize = if ($collector.LocalName -eq 'HeapEventCollector') { '65536' } else { '16384' }
    if (!$collector.StackCaching -or $collector.StackCaching.BucketCount -ne $buckets -or $collector.StackCaching.CacheSize -ne $cacheSize) { throw 'Native stack cache is absent or unbounded' }
  }
}
foreach ($document in @($before,$after)) {
  foreach ($node in @($document.SelectNodes('//BufferSize | //Buffers | //StackCaching'))) { [void]$node.ParentNode.RemoveChild($node) }
}
if ($before.DocumentElement.OuterXml -cne $after.DocumentElement.OuterXml) { throw 'Native provider/profile/stack semantics changed' }
";
            let preserved = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", CHECK])
                .env("BEND_PERF_WPR_SOURCE", &source)
                .env("BEND_PERF_WPR_PROFILE", &profile)
                .output()?;
            assert!(preserved.status.success(), "{preserved:?}");
            let details = std::process::Command::new("wpr")
                .args(["-profiledetails"])
                .arg(format!("{}!{name}", profile.display()))
                .arg("-filemode")
                .output()?;
            assert!(details.status.success(), "{details:?}");
        }
        Ok(())
    }
}
