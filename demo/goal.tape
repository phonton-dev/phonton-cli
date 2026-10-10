Output out/goal-TAKE.mp4

Set Shell "bash"
Set FontFamily "JetBrains Mono"
Set FontSize 18
Set Width 1920
Set Height 1080
Set Padding 24
Set TypingSpeed 30ms
Set WaitTimeout 900s
Set Theme { "name": "phonton", "black": "#1b1f2a", "red": "#f07178", "green": "#7fd88f", "yellow": "#f5a742", "blue": "#5ab0f6", "magenta": "#c08cf5", "cyan": "#5fd7d7", "white": "#c8ced9", "brightBlack": "#6b7385", "brightRed": "#ff8a92", "brightGreen": "#9be8a8", "brightYellow": "#ffc46b", "brightBlue": "#82c4ff", "brightMagenta": "#d6a9ff", "brightCyan": "#8eeaea", "brightWhite": "#eef1f6", "background": "#161a23", "foreground": "#c8ced9", "selection": "#2a3142", "cursor": "#82c4ff" }

Hide
Type "test -f $PHONTON_HOME/config.toml || exit 1; export PS1='$ '; cd $DEMO_WORK/port-config; clear; phonton"
Enter
Wait+Screen /Trust this folder/
Type "y"
Enter
Wait+Screen /GOAL/
Sleep 1.5s
Show

Sleep 1.5s
Type "Make parsePort in @src/port.js reject ports outside 1-65535 and trailing garbage so npm test passes"
Sleep 700ms
Enter
Wait+Screen /Run checks on this machine/
Sleep 2s
Type "Y"
Wait+Screen /Review local plan/
Sleep 3.5s
Screenshot out/goal-TAKE-plan.png
Type "Y"
Wait+Screen /VERIFIED|Streak reset|Run record unchanged|receipt/
Sleep 2s
Screenshot out/goal-TAKE-end.png
Sleep 4s
