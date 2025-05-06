import { queryScanSettings } from '@/services/dfr/queryScanSettings';
import { queryScanStatus } from '@/services/dfr/queryScanStatus';
import { startScan } from '@/services/dfr/startScan';
import { stopScan } from '@/services/dfr/stopScan';
import { formatSize } from '@/utils/format_utils';
import { PageContainer, ProForm, ProFormDigit, ProFormInstance, ProFormSelect, ProFormText, ProFormTextArea } from '@ant-design/pro-components';
import { useIntl } from '@umijs/max';
import { Button, Card, Col, message, Row, Space, Typography } from 'antd';
import React, { useEffect, useRef, useState } from 'react';
const { Title, Paragraph, Text, Link } = Typography;
const Admin: React.FC = () => {
  const intl = useIntl();
  // scaning state to track if the scan is running or not
  const [scaning, setScaning] = useState<boolean>(false);
  //const [timerId, setTimerId] = useState<NodeJS.Timeout | null>(null);
  const [scanStatus, setScanStatus] = useState<API.RestResponseScanStatus | null>(null);
  const scaningRef = useRef(scaning);
  const timerIdRef = useRef<NodeJS.Timeout | null>(null);

  useEffect(() => {
    scaningRef.current = scaning;
  }, [scaning]);

  useEffect(() => {
    (async () => {
      const scan_status = await queryScanStatus();
      console.log('当前进度:', scan_status);
      setScanStatus(scan_status);
      setScaning(!!scan_status.data?.started);
    })();
  }, []);

  useEffect(() => {
    async function requestScanStatus() {
      // 定时器执行的代码
      // console.info("定时器执行中...")
      var timeout = 3000; // 设置定时器间隔为 3 秒
      if (scaningRef.current) { // 只有在扫描进行中时才执行获取进度的操作
        timeout = 200; // 如果扫描进行中，则将定时器间隔设置为 200 毫秒
        const scan_status = await queryScanStatus();
        console.log('当前进度:', scan_status);
        setScanStatus(scan_status);
        if (!scan_status.data?.started) {
          setScaning(false); // 如果扫描已结束，则设置 scaning 为 false
        }
      }
      const id = setTimeout(requestScanStatus, timeout);
      //setTimerId(id);
      timerIdRef.current = id;
    }
    const id = setTimeout(requestScanStatus, 0);
    //setTimerId(id);
    timerIdRef.current = id;

    return () => {
      /*
      if (timerId) {
        clearTimeout(timerId); // 组件卸载时清除定时器
      }
        */
      if (timerIdRef.current) {
        clearTimeout(timerIdRef.current); // 组件卸载时清除定时器
      }
    };
  }, []);
  const formRef = useRef<
    ProFormInstance<API.ScanSettings>
  >();
  return (
    <PageContainer>
      <Card>
        <Space>
          <Button
            type="primary"
            disabled={!scaning}
            onClick={async () => {
              const result = await stopScan();
              console.log('stopScan result:', result);
              message.success(intl.formatMessage({ id: "pages.scan.scanStoppedMessage" }));
              setScaning(false);
            }}
          >{intl.formatMessage({ id: 'pages.searchTable.stopSearch' })}</Button>
        </Space>

        <Row>
          <Col span={8}>{intl.formatMessage({ id: "pages.scan.scanedFileCount" })}</Col>
          <Col span={16}>{scanStatus?.data?.scanned_file_count}</Col>
        </Row>
        <Row>
          <Col span={8}>{intl.formatMessage({ id: "pages.scan.currentDir" })}</Col>
          <Col span={16}>{scanStatus?.data?.current_file_info?.dir_path}</Col>
        </Row>
        <Row>
          <Col span={8}>{intl.formatMessage({ id: "pages.scan.currentFile" })}</Col>
          <Col span={16}>{scanStatus?.data?.current_file_info?.file_name}</Col>
        </Row>
        <Row>
          <Col span={8}>{intl.formatMessage({ id: "pages.scan.fileSize" })}</Col>
          <Col span={16}>{formatSize(scanStatus?.data?.current_file_info?.inode_info.size)}</Col>
        </Row>

      </Card>
      <Card>
        <ProForm<API.ScanSettings & {
          ignore_path_area?: string
        }>
          disabled={scaning}
          submitter={{
            searchConfig: {
              submitText: intl.formatMessage({ id: 'pages.searchTable.startSearch' }),
            }
          }}
          request={async () => {
            const response = await queryScanSettings();
            let ignore_path_area = null;
            if (response.data?.ignore_paths != null) {
              ignore_path_area = response.data?.ignore_paths.join("\n");
            }
            return {
              ignore_path_area,
              ...response.data!
            };
          }
          }
          onFinish={async (values) => {
            console.log('ProForm values: ', values);
            const val1 = await formRef.current?.validateFields();
            console.log('validateFields:', val1);
            const val2 = await formRef.current?.validateFieldsReturnFormatValue?.();
            console.log('validateFieldsReturnFormatValue:', val2);

            if (values.ignore_path_area) {
              values.ignore_paths = values.ignore_path_area.split("\n");
            }
            let request = {
              ...values,
            }
            delete request.ignore_path_area;
            console.log('startScan request:', request);
            const result = await startScan(request);
            setScaning(true);
            console.log('startScan result:', result);

            message.success(intl.formatMessage({ id: 'pages.scan.scanStartMessage' }));
          }}
        >
          <ProFormText name="scan_path" label={intl.formatMessage({ id: "pages.scan.dirPathToScan" })} />
          <ProFormSelect name="include_file_extensions"
            label={intl.formatMessage({ id: "pages.scan.fileExtentionListToScan" })}
            mode='tags'
            request={async (params) => {
              console.log("ProFormSelect request:", params)
              return [
                { label: "jpg", value: 'jpg' },
                { label: 'bmp', value: 'bmp' },
                { label: 'png', value: 'png' },
                { label: 'heic', value: 'heic' },
                { label: 'mp4', value: 'mp4' },
                { label: 'avi', value: 'avi' },
                { label: 'mov', value: 'mov' },
                { label: 'pdf', value: 'pdf' },
              ];
            }}
          />
          <ProFormDigit label={intl.formatMessage({ id: "pages.scan.minFileSizeToScan" })} name="min_file_size" min={0} />
          <ProFormDigit label={intl.formatMessage({ id: "pages.scan.maxFileSizeToScan" })} name="max_file_size" min={0} />
          <ProFormTextArea
            name="ignore_path_area"
            label={intl.formatMessage({ id: "pages.scan.excludeDirList" })}
            placeholder={intl.formatMessage({ id: "pages.scan.excludeDirListPlaceholder" })}
          />
        </ProForm>
      </Card>
    </PageContainer>
  );
};

export default Admin;
