#pragma once
#include <QList>
#include <QPointer>
#include <QString>
#include <QWidget>

class PermissionBar;
class PromptInput;
class TranscriptModel;
class QJsonObject;
class QLabel;
class QPushButton;
class QScrollArea;
class QVBoxLayout;

/// One Claude agent's conversation: the frames, the permission bar and the
/// prompt box.
///
/// The view paints and forwards, and decides nothing. What a frame says, which
/// tool is waiting, what the turn cost and whether a card is folded are all
/// read back out of the `TranscriptModel`; Allow, Deny, Interrupt, Stop and a
/// typed prompt go straight back into it. There is no "always allow" rule here
/// and no cost arithmetic: the model answers a tool it has been told to allow
/// without this view ever hearing about it.
///
/// Frames are built once and updated in place. `resetItems` is the only thing
/// that rebuilds, which is what makes replaying a long history one layout pass
/// rather than one per message.
class TranscriptView : public QWidget {
    Q_OBJECT
public:
    /// Does not take ownership of `model`: the area owns both and tears them
    /// down together.
    explicit TranscriptView(TranscriptModel* model, QWidget* parent = nullptr);

    TranscriptModel* model() const;

private:
    void rebuild();
    void onItemAppended(int index);
    void onItemChanged(int index);
    void onPermissionRequested();
    void onBusyChanged();
    void onStateChanged();
    /// Puts `message` in the banner, or takes it down when both the daemon's
    /// state and the last request are clean.
    void refreshBanner();

    /// Builds the frame for one item, connecting a tool card's toggle back to
    /// the model at `index`.
    QWidget* makeFrame(const QJsonObject& item, int index);
    /// Repaints the frame at `index`, or replaces it if the item is no longer
    /// the kind that frame was built for.
    void updateFrame(int index, const QJsonObject& item);
    /// Drops every frame. The stretch at the foot of the column stays.
    void clearFrames();
    /// One item as an object, or an empty object for an index the model does
    /// not have.
    QJsonObject itemAt(int index) const;
    /// Whether the view is within one line of the foot of the scroll.
    bool atBottom() const;

    QPointer<TranscriptModel> m_model;
    QScrollArea* m_scroll = nullptr;
    /// The column of frames, with a stretch as its last entry.
    QVBoxLayout* m_frameLayout = nullptr;
    QList<QWidget*> m_frames;
    QLabel* m_banner = nullptr;
    PermissionBar* m_permission = nullptr;
    PromptInput* m_input = nullptr;
    QPushButton* m_interrupt = nullptr;
    QPushButton* m_stop = nullptr;
    /// Whether the next range change should jump to the foot. Recomputed from
    /// the scroll position before every append, so a user who has scrolled up
    /// to read is left where they are.
    bool m_stickToBottom = true;
    /// The last `errorOccurred` message, cleared when the user sends again.
    QString m_requestError;
};
