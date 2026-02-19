import { querySettings } from "@/services/desk/querySettings";
import { updateSettings } from "@/services/desk/updateSettings";
import { PageContainer, ProForm, ProFormDigit, ProFormSelect, ProFormSwitch, ProFormText } from "@ant-design/pro-components";
import { useIntl, useModel } from "@umijs/max";
import { Alert, Divider, message } from "antd";



const Settings: React.FC = () => {
    const { initialState, setInitialState } = useModel('@@initialState');
    const intl = useIntl();
    return (
        <PageContainer>
            <Alert
                message={intl.formatMessage({ id: "pages.system.settings.alert.message" })}
                description={intl.formatMessage({ id: "pages.system.settings.alert.description" })}
                type="warning"
                showIcon
            />
            <Divider />
            <ProForm<API.SystemSettings>
                onValuesChange={(changeValues) => console.log(changeValues)}

                request={async () => {
                    const response = await querySettings();
                    return response.data!;
                }}
                onFinish={async (values) => {
                    console.log(values);
                    await updateSettings(values);
                    message.success(intl.formatMessage({ id: "pages.system.settings.updateSucceedMessage" }));
                }}
            >
                <ProFormSwitch
                    name="open_browser_on_startup"
                    label={intl.formatMessage({
                        id: "pages.system.settings.openBrowserOnStartup",
                    })}
                />
                <ProFormSwitch
                    name="enable_ipv6"
                    label={intl.formatMessage({
                        id: "pages.system.settings.enableIpv6",
                    })}
                />
                <ProFormSwitch
                    name="traceback"
                    label={intl.formatMessage({
                        id: 'pages.system.settings.traceback',
                    })}
                />
                <ProFormSwitch
                    name="telemetry_consent"
                    label={intl.formatMessage({
                        id: 'pages.system.settings.telemetry_consent',
                        defaultMessage: 'Telemetry Consent',
                    })}
                    tooltip={intl.formatMessage({
                        id: 'pages.system.settings.telemetry_consent.tooltip',
                        defaultMessage: 'Help improve our product by sending anonymous usage data.',
                    })}
                />
                <ProFormText
                    name="listen_addr_ipv4"
                    label={intl.formatMessage({
                        id: "pages.system.settings.listenAddrIpv4",
                    })}
                    hasFeedback
                    rules={[
                        { required: true, message: intl.formatMessage({ id: "pages.system.settings.ipv4AddressRequiredMessage" }) },
                    ]}
                />
                <ProFormText
                    name="listen_addr_ipv6"
                    label={intl.formatMessage({
                        id: "pages.system.settings.listenAddrIpv6",
                    })} />
                <ProFormText
                    name="signaling_url"
                    label={intl.formatMessage({
                        id: "pages.system.settings.signalingUrl",
                    })}
                />
                <ProFormDigit
                    label={intl.formatMessage({
                        id: "pages.system.settings.port",
                    })}
                    name="port"
                    min={1}
                    max={65535}
                    fieldProps={{ precision: 0 }}
                />

                <ProFormSelect
                    name="log_level"
                    label={
                        intl.formatMessage({
                            id: "pages.system.settings.logLevel",
                        })
                    }
                    valueEnum={{
                        trace: "TRACE",
                        debug: 'DEBUG',
                        info: 'INFO',
                        warn: 'WARN',
                        error: 'ERROR',
                    }}
                    placeholder={intl.formatMessage({ id: 'pages.system.settings.logLevelRequiredMessage' })}
                    rules={[{ required: true, message: intl.formatMessage({ id: 'pages.system.settings.logLevelRequiredMessage' }) }]}
                />


            </ProForm>
        </PageContainer>

    );
}

export default Settings;