#pragma once
#include <QFrame>
#include <QJsonArray>
#include <QString>

class QEvent;
class QTimer;
class QVBoxLayout;

/// The strip above the prompt box listing the connections the workspace's
/// proxy is holding for an answer: one row per host, with how long the proxy
/// will go on waiting, and Allow / Always block.
///
/// It sat inline in the transcript once, where it scrolled away under the
/// agent's output while the proxy held the request. Here it stays put until
/// the request is answered or its time is up, like the permission bar below
/// it.
///
/// It decides nothing and keeps nothing: every row comes from the model's
/// `networkJson`, and every button is a signal. The one thing it does on its
/// own is tick once a second, to redraw the countdowns and ask the model to
/// drop the rows whose time is up (`expireRequested`).
class NetworkBar : public QFrame {
    Q_OBJECT
public:
    explicit NetworkBar(QWidget* parent = nullptr);

    /// Shows `rows`, one `networkJson` array from `TranscriptModel`, and
    /// hides the bar when it is empty.
    void setRequests(const QJsonArray& rows);

Q_SIGNALS:
    void allowRequested(const QString& host);
    void blockRequested(const QString& host);
    void dismissRequested(const QString& host);
    /// Once a second while there are rows: time for the model to drop the
    /// ones that have run out.
    void expireRequested();

protected:
    /// The amber is mixed into the pane's own background, so a theme change
    /// has to mix it again; see `PermissionBar`.
    void changeEvent(QEvent* event) override;

private:
    void rebuild();
    void tick();
    void applyWash();
    /// What row `row` says, at this moment.
    static QString rowText(const QJsonObject& row, qint64 nowMs);

    QVBoxLayout* m_rows = nullptr;
    QJsonArray m_requests;
    QTimer* m_tick = nullptr;
    bool m_mixing = false;
};
