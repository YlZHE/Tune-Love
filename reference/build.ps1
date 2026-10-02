param([ValidateSet('tests','identification','agent','fixture','probe','fault','all')][string]$Target='all',
      [ValidateSet('x64','x86')][string]$Architecture='x64')
$ErrorActionPreference='Stop'
$refRoot=$PSScriptRoot
$vendorRoot=Join-Path $refRoot 'vendor'
if(-not (Test-Path (Join-Path $vendorRoot 'pluginterfaces/vst/ivstaudioprocessor.h')) -or
   -not (Test-Path (Join-Path $vendorRoot 'minhook/src/hook.c'))){
    throw 'Run reference/bootstrap.ps1 to obtain the pinned public dependencies first'
}
$buildRoot=Join-Path $refRoot "build/$Architecture"
New-Item -ItemType Directory -Path $buildRoot -Force | Out-Null
$vswherePath='C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe'
$vsPath=& $vswherePath -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if(-not $vsPath){throw 'MSVC tools are required'}
$vcvarsPath=Join-Path $vsPath 'VC/Auxiliary/Build/vcvarsall.bat'
$vcEnv=& $env:ComSpec /d /s /c ('call "'+$vcvarsPath+'" '+$Architecture+' >nul && set')
if($LASTEXITCODE -ne 0){throw 'MSVC environment setup failed'}
foreach($vcLine in $vcEnv){if($vcLine -match '^([^=]+)=(.*)$'){[Environment]::SetEnvironmentVariable($matches[1],$matches[2],'Process')}}
$cpp=@('/nologo','/std:c++20','/EHsc','/W4','/MT','/O2','/DUNICODE','/D_UNICODE',"/I$vendorRoot")
Push-Location $buildRoot
try {
    if($Target -in @('tests','all')){
        & cl @cpp "$refRoot/tests/delivery_tests.cpp" /Fe:delivery_tests.exe
        if($LASTEXITCODE -ne 0){throw 'Delivery tests compilation failed'}
        & ./delivery_tests.exe
        if($LASTEXITCODE -ne 0){throw 'Delivery tests failed'}
    }
    if($Target -in @('tests','identification','agent','fault','all')){
        $minRoot=Join-Path $vendorRoot 'minhook'
        $hde=if($Architecture -eq 'x64'){'hde64'}else{'hde32'}
        & cl /nologo /c /O2 /MT /W3 "/I$minRoot/include" "/I$minRoot/src" "$minRoot/src/buffer.c" "$minRoot/src/hook.c" "$minRoot/src/trampoline.c" "$minRoot/src/hde/$hde.c"
        if($LASTEXITCODE -ne 0){throw 'MinHook compilation failed'}
    }
    if($Target -in @('tests','identification','all')){
        & cl @cpp "/I$minRoot/include" "$refRoot/tests/identification_tests.cpp" buffer.obj hook.obj trampoline.obj "$hde.obj" user32.lib advapi32.lib ole32.lib /Fe:identification_tests.exe
        if($LASTEXITCODE -ne 0){throw 'Identification tests compilation failed'}
        & ./identification_tests.exe
        if($LASTEXITCODE -ne 0){throw 'Identification tests failed'}
    }
    if($Target -in @('agent','fault','all')){
        $agentOutput=if($Target -eq 'fault'){'reference_agent_fault.dll'}else{'reference_agent.dll'}
        [string[]]$agentFlags=@()
        if($Target -eq 'fault'){$agentFlags+= '/DATR_TEST_FAULTS'}
        & cl @cpp @agentFlags /LD "/I$minRoot/include" "$refRoot/native/agent.cpp" buffer.obj hook.obj trampoline.obj "$hde.obj" user32.lib advapi32.lib ole32.lib "/Fe:$agentOutput"
        if($LASTEXITCODE -ne 0){throw 'Agent compilation failed'}
    }
    if($Target -in @('fixture','all')){
        & cl @cpp /LD /DFIXTURE_DLL "$refRoot/tests/fixture.cpp" /Fe:ReferenceFixture.vst3
        if($LASTEXITCODE -ne 0){throw 'Fixture compilation failed'}
        & cl @cpp /LD /DFIXTURE_DLL /DALT_PROFILE "$refRoot/tests/fixture.cpp" /Fe:ReferenceFixtureAlt.vst3
        if($LASTEXITCODE -ne 0){throw 'Alternate fixture compilation failed'}
        & cl @cpp "$refRoot/tests/fixture.cpp" user32.lib /Fe:reference_host.exe
        if($LASTEXITCODE -ne 0){throw 'Host compilation failed'}
    }
    if($Target -in @('probe','all')){
        & cl @cpp "$refRoot/native/describe.cpp" ole32.lib /Fe:describe.exe
        if($LASTEXITCODE -ne 0){throw 'Describe compilation failed'}
    }
} finally {Pop-Location}
