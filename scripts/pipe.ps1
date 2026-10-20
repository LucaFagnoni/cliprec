# Invia richieste al core di ClipRec sulla named pipe e stampa le risposte.
# Uso: scripts\pipe.ps1 Status TestClip Quit
param([Parameter(ValueFromRemainingArguments)][string[]]$Requests = @('Status'))
$name = "cliprec-$env:USERNAME"
$pipe = [System.IO.Pipes.NamedPipeClientStream]::new('.', $name, [System.IO.Pipes.PipeDirection]::InOut)
$pipe.Connect(3000)
$writer = [System.IO.StreamWriter]::new($pipe); $writer.AutoFlush = $true
$reader = [System.IO.StreamReader]::new($pipe)
foreach ($r in $Requests) {
    $writer.Write("`"$r`"`n")
    "$r -> $($reader.ReadLine())"
}
$pipe.Dispose()
