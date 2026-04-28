# shellcheck shell=bash

Describe "Management CLI basics"
  Describe "--version"
    It "exits 0 and prints the binary name"
      When run command git-remote-object-store --version
      The status should equal 0
      The output should include "git-remote-object-store"
    End
  End

  Describe "--help"
    It "exits 0 and prints usage"
      When run command git-remote-object-store --help
      The status should equal 0
      The output should include "Usage:"
    End
  End
End
