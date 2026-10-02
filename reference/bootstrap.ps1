$ErrorActionPreference='Stop'
$dependencies=@(
    @{Name='pluginterfaces'; Url='https://github.com/steinbergmedia/vst3_pluginterfaces.git'; Commit='4f547e8e102b47de4a8b8aaf343c73b700786372'},
    @{Name='minhook'; Url='https://github.com/TsudaKageyu/minhook.git'; Commit='c3fcafdc10146beb5919319d0683e44e3c30d537'}
)
foreach($dependency in $dependencies){
    $destination=Join-Path $PSScriptRoot ('vendor/'+$dependency.Name)
    if(-not (Test-Path -LiteralPath $destination)){
        & git clone $dependency.Url $destination
        if($LASTEXITCODE -ne 0){throw ('Cannot clone '+$dependency.Name)}
        & git -C $destination checkout --detach $dependency.Commit
        if($LASTEXITCODE -ne 0){throw ('Cannot select pinned revision for '+$dependency.Name)}
    }
    $actual=& git -C $destination rev-parse HEAD
    if($LASTEXITCODE -ne 0 -or $actual -ne $dependency.Commit){throw ('Dependency revision mismatch; left untouched: '+$destination)}
    $changes=& git -C $destination status --porcelain
    if($LASTEXITCODE -ne 0 -or $changes){throw ('Dependency checkout is modified; left untouched: '+$destination)}
    Write-Output ($dependency.Name+' '+$actual)
}
