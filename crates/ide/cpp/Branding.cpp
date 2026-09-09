#include "Branding.h"
#include "LogoData.h"

#ifndef BS_IDE_VERSION
#define BS_IDE_VERSION "unknown"
#endif

namespace branding {

static QPixmap logo256() {
    static const QPixmap cached = [] {
        QPixmap pm;
        pm.loadFromData(kLogoPng256, static_cast<uint>(kLogoPng256Size), "PNG");
        return pm;
    }();
    return cached;
}

QPixmap logo(int size) {
    return logo256().scaled(size, size, Qt::KeepAspectRatio, Qt::FastTransformation);
}

QIcon appIcon() {
    QIcon icon;
    for (int size : {16, 32, 48, 64, 128, 256}) {
        icon.addPixmap(logo(size));
    }
    return icon;
}

QString version() {
    return QStringLiteral(BS_IDE_VERSION);
}

}  // namespace branding
