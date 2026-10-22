#pragma once
#include <QJsonArray>
#include <QJsonObject>
#include <QHash>
#include <QWidget>

class AppController;
class QComboBox;
class QLineEdit;
class QLabel;
class QTableWidget;

/// Ordinary OpenCode provider editor. A provider's key is never read into a
/// widget: an empty password field preserves its Windows credential entry.
class ProviderPage : public QWidget {
    Q_OBJECT
public:
    explicit ProviderPage(AppController *controller, QWidget *parent = nullptr);
    QJsonArray providers() const;
#if defined(BS_WIDGET_TESTS)
    qint64 requestSerialForTest() const { return m_pending; }
    QString pendingCredentialProviderForTest(qint64 serial) const { return m_pendingKeys.value(serial).first; }
#endif
signals:
    void providersChanged(const QJsonArray &providers);
private:
    void load();
    void showProvider(const QString &id);
    void save();
    void remove();
    void preset(const QString &name);
    void finished(const QString &method, qint64 serial, const QString &json, const QString &error);
    qint64 call(const QString &method, const QJsonObject &params);
    AppController *m_controller;
    QComboBox *m_choice;
    QLineEdit *m_id;
    QLineEdit *m_name;
    QLineEdit *m_url;
    QComboBox *m_target;
    QTableWidget *m_models;
    QLineEdit *m_key;
    QLabel *m_status;
    QJsonArray m_providers;
    qint64 m_revision = 0;
    qint64 m_serial = 0;
    qint64 m_pending = 0;
    QString m_pendingMethod;
    QHash<qint64, QPair<QString, QString>> m_pendingKeys;
    QHash<qint64, QString> m_pendingRemovals;
};
