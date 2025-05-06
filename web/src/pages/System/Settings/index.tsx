import { querySettings } from "@/services/dfr/querySettings";
import { updateSettings } from "@/services/dfr/updateSettings";
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
                <ProFormText
                    name="config_file_path"
                    label={intl.formatMessage({
                        id: "pages.system.settings.configFilePath",
                    })}
                    disabled />
                <ProFormText
                    name="db_path"
                    label={intl.formatMessage({
                        id: "pages.system.settings.dbPath",
                    })}
                    rules={[
                        { required: true, message: intl.formatMessage({ id: "pages.system.settings.sqlitePathRequiredMessage" }) },
                    ]}
                    hasFeedback
                />
                <ProFormSwitch
                    name="enable_ipv6"
                    label={intl.formatMessage({
                        id: "pages.system.settings.enableIpv6",
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

                <ProFormDigit
                    label={intl.formatMessage({
                        id: "pages.system.settings.clearTrashIntervalS",
                    })}
                    name="clear_trash_interval_s"
                    min={1}
                    fieldProps={{ precision: 0 }}
                />

                <ProFormText
                    name="trash_path"
                    label={intl.formatMessage({
                        id: "pages.system.settings.trashPath",
                    })}
                    disabled />
            </ProForm>
        </PageContainer>

    );
}

export default Settings;