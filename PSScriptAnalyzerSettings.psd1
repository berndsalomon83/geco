@{
    Severity = @('Error', 'Warning')

    ExcludeRules = @(
        # Write-Host is the right call here and Write-Output would be wrong.
        # install.ps1 / uninstall.ps1 are interactive installers: their
        # progress lines are coloured output for a human at a console, not
        # data. Write-Output would put them on the pipeline, where a caller
        # capturing the script's output would swallow them and where they'd
        # contaminate any value the script returned.
        'PSAvoidUsingWriteHost'
    )

    # NOTE: PSUseBOMForUnicodeEncodedFile is deliberately LEFT ON. Windows
    # PowerShell 5.1 reads a BOM-less file as ANSI, so a non-ASCII character
    # in a .ps1 is silently decoded wrongly. Keeping the rule enabled enforces the
    # simpler invariant: these scripts stay pure ASCII, which behaves
    # identically under 5.1 and 7 with no BOM to get lost in transit.
}
