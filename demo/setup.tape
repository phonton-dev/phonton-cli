Output out/setup.mp4

Set Shell "bash"
Set FontFamily "JetBrains Mono"
Set FontSize 20
Set Width 1920
Set Height 1080
Set Padding 32
Set TypingSpeed 45ms
Set WaitTimeout 1200s
Set Theme { "name": "phonton", "black": "#1b1f2a", "red": "#f07178", "green": "#7fd88f", "yellow": "#f5a742", "blue": "#5ab0f6", "magenta": "#c08cf5", "cyan": "#5fd7d7", "white": "#c8ced9", "brightBlack": "#6b7385", "brightRed": "#ff8a92", "brightGreen": "#9be8a8", "brightYellow": "#ffc46b", "brightBlue": "#82c4ff", "brightMagenta": "#d6a9ff", "brightCyan": "#8eeaea", "brightWhite": "#eef1f6", "background": "#161a23", "foreground": "#c8ced9", "selection": "#2a3142", "cursor": "#82c4ff" }

Hide
Type "test -f $PHONTON_HOME/config.toml || exit 1; export PS1='$ '; cd $DEMO_WORK; clear"
Enter
Sleep 1s
Show
Sleep 1s
Type "phonton models setup qwen2.5-coder:3b"
Sleep 700ms
Enter
Wait+Screen /Ready:/
Sleep 4s
Screenshot out/setup-end.png
Sleep 1s
