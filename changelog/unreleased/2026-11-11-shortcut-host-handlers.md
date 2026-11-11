- **A closing shortcut host leaves a later host's handlers alone.** Closing used to clear every
  handler in the App, so a host that attached while the previous one was closing could
  register a chord successfully and then have it do nothing when pressed. Each registration now
  records the host that took it, and a close removes the handlers of that host and those it
  replaced. A later host's handlers stay, as do those of registrations no host has taken yet.
