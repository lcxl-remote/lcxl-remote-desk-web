import { updateTelemetryConsent } from '@/services/desk/updateTelemetryConsent';
import { useIntl } from '@umijs/max';
import { Button, Checkbox, Modal, Typography } from 'antd';
import React, { useState } from 'react';

const { Paragraph, Title } = Typography;

interface WelcomeModalProps {
    open: boolean;
    onClose: () => void;
}

const WelcomeModal: React.FC<WelcomeModalProps> = ({ open, onClose }) => {
    const intl = useIntl();
    const [consent, setConsent] = useState(false);
    const [loading, setLoading] = useState(false);

    const handleOk = async () => {
        setLoading(true);
        try {
            await updateTelemetryConsent({ consent });
            onClose();
        } catch (error) {
            console.error('Failed to update telemetry consent', error);
        } finally {
            setLoading(false);
        }
    };

    return (
        <Modal
            open={open}
            title={intl.formatMessage({ id: 'component.welcomeModal.title', defaultMessage: 'Welcome to Lcxl Remote Desk' })}
            footer={[
                <Button key="submit" type="primary" loading={loading} onClick={handleOk}>
                    {intl.formatMessage({ id: 'component.welcomeModal.pbutton', defaultMessage: 'Get Started' })}
                </Button>,
            ]}
            closable={false}
            maskClosable={false}
        >
            <div style={{ textAlign: 'center' }}>
                <Title level={3}>{intl.formatMessage({ id: 'component.welcomeModal.welcome', defaultMessage: 'Welcome!' })}</Title>
                <Paragraph>
                    {intl.formatMessage({ id: 'component.welcomeModal.description', defaultMessage: 'Thank you for using Lcxl Remote Desk. We are constantly improving our product.' })}
                </Paragraph>
                <div style={{ marginTop: 24, marginBottom: 24 }}>
                    <Checkbox checked={consent} onChange={(e) => setConsent(e.target.checked)}>
                        {intl.formatMessage({ id: 'component.welcomeModal.consent', defaultMessage: 'Help improve our product by sending anonymous usage data.' })}
                    </Checkbox>
                </div>
                <Paragraph type="secondary" style={{ fontSize: 12 }}>
                    {intl.formatMessage({ id: 'component.welcomeModal.privacy', defaultMessage: 'We only collect system information (OS, CPU, RAM) and application logs. No personal data is collected.' })}
                </Paragraph>
            </div>
        </Modal>
    );
};

export default WelcomeModal;
