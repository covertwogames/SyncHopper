# SyncHopper

SyncHopper is a Windows app for your cloud storage accounts. It keeps the folders you choose in step between your PC and the cloud, with one app for all of your accounts.

Under the hood it is a front end for [rclone](https://rclone.org), the open-source sync engine that works with dozens of storage providers. SyncHopper gives it a setup you can click through, a window that shows what is happening, and safety checks around deletes.

**This is a test version, and right now it works with [Koofr](https://koofr.eu) only.** More providers are coming soon.

It has been working well for us, but it passes on deletions as well as changes, so start with a folder you could afford to lose and keep a backup of anything important.

## What it does today, with Koofr

- **Synced folders.** Add, change or delete a file on your PC or in Koofr and the other side follows.
- **Koofr drive.** Your whole Koofr account as a drive letter, without keeping the files on your PC. This one is optional.
- **Upload to Koofr.** Send big files and folders straight to Koofr without syncing them.
- **Right-click in File Explorer** to send something to Koofr or start syncing a folder.
- **Careful with deletes.** It asks before passing on a large delete and never empties a whole folder by itself.
- **Updates itself.**

## Install

1. Download [SyncHopper-setup.exe](https://github.com/covertwogames/SyncHopper/releases/latest/download/SyncHopper-setup.exe).
2. Run it. Because the app is new, Windows may say "Windows protected your PC". Click **More info**, check that the publisher is Cover Two Strategies LLC, then click **Run anyway**.
3. Follow the setup. You need your Koofr email and a Koofr app password, which is not your normal Koofr password. Setup has a button that opens the Koofr page where you create one.

SyncHopper needs Windows 10 or 11, 64-bit. Most of our testing has been on Windows 11.

## Report a problem

Please [open an issue](https://github.com/covertwogames/SyncHopper/issues). It helps a lot if you include:

- what you were doing and what you expected to happen
- what SyncHopper showed instead (a screenshot is fine)
- the text from **Settings**, **Advanced settings**, **Copy diagnostic info**

The diagnostic text has no passwords in it and your email address is partly hidden. It does list the folders you sync and the names of any files that failed, and issues are public, so take out anything you would rather not post.

## Good to know

- Your files and your sign-in stay on your PC. SyncHopper connects to your storage provider, and to GitHub to check for updates. Nothing is sent to us.
- Turning on the Koofr drive downloads a small free driver called WinFsp.
- rclone is free, open-source software by Nick Craig-Wood and the rclone contributors. SyncHopper downloads it for you and keeps it up to date.
- To remove SyncHopper, uninstall it from Windows Settings, Apps. Your files stay where they are, on your PC and in the cloud.

SyncHopper is made by Cover Two Games. It is not made by or affiliated with rclone, Koofr or any other storage provider.
