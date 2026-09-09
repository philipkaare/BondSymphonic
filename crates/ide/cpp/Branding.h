#pragma once
#include <QIcon>
#include <QPixmap>
#include <QString>

// Application identity shared by the window icon and the About box. The logo
// is compiled in (LogoData.h) so the binary needs no asset files next to it.
namespace branding {

/// The logo scaled to `size` px with nearest-neighbour so the pixel art stays crisp.
QPixmap logo(int size);

/// Window/taskbar icon: the logo at 256 px, letting Qt pick smaller sizes.
QIcon appIcon();

/// The IDE version compiled in from Cargo.toml.
QString version();

}  // namespace branding
