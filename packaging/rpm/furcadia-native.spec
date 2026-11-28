%global debug_package %{nil}

Name:           furcadia-native
Version:        2.2.6
Release:        1%{?dist}
Summary:        Native Furcadia client (GUI and CLI)
License:        CC-BY-NC-SA-4.0
URL:            https://github.com/morganlowe/native-furcadia
Requires:       curl, bzip2, unzip

%description
A fast, standalone native Rust client for Furcadia supporting modern and
legacy dreams, custom portraits, Pounce watchlist, whisper tabs, DragonSpeak,
and Windows Furcadia asset import.

%install
install -Dm755 "%{_srcroot}/target/release/furc-gui" "%{buildroot}%{_bindir}/furc-gui"
install -Dm755 "%{_srcroot}/target/release/furc-cli" "%{buildroot}%{_bindir}/furc-cli"
install -Dm644 "%{_srcroot}/packaging/linux/furcadia-native.desktop" "%{buildroot}%{_datadir}/applications/furcadia-native.desktop"
install -Dm644 "%{_srcroot}/packaging/linux/furcadia-native.png" "%{buildroot}%{_datadir}/icons/hicolor/256x256/apps/furcadia-native.png"
install -Dm644 "%{_srcroot}/LICENSE" "%{buildroot}%{_datadir}/licenses/%{name}/LICENSE"

%files
%{_bindir}/furc-gui
%{_bindir}/furc-cli
%{_datadir}/applications/furcadia-native.desktop
%{_datadir}/icons/hicolor/256x256/apps/furcadia-native.png
%dir %{_datadir}/licenses/%{name}
%{_datadir}/licenses/%{name}/LICENSE
